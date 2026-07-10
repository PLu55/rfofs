/// All parameters needed to fully describe a FOF grain.
///
/// This struct is `Copy` and heap-free — safe to send through lock-free queues.
///
/// # ID convention
/// - `id == 0`  → fire-and-forget; not tracked in the id map.
/// - `id != 0`  → caller-assigned stable id; eligible for targeted kill.
#[derive(Clone, Copy, Debug)]
pub struct FofParams {
    /// Caller-assigned id. 0 = fire-and-forget.
    pub id: u64,

    /// Absolute sample clock at which this FOF begins its attack.
    /// Sub-block accurate: the engine will offset within the current block.
    pub start_sample: u64,

    // ── Carrier ──────────────────────────────────────────────────────────────

    /// Formant frequency at grain start (Hz).
    pub f: f32,

    /// Glissando rate in octaves/second.  0.0 = no pitch change.
    /// Positive = upward, negative = downward.
    pub gliss: f32,

    /// Initial sine phase (radians).
    pub phi: f32,

    /// Peak amplitude (envelope maximum).
    pub amp: f32,

    // ── Envelope ─────────────────────────────────────────────────────────────

    /// Exponential decay coefficient α in s⁻¹.  env_decay(t) = exp(−α·t), t in seconds.
    pub alpha: f32,

    /// Attack duration β in seconds (half-cosine sigmoid).
    pub beta: f32,

    // ── Natural fade-out ─────────────────────────────────────────────────────

    /// Threshold relative to `amp` at which the natural fade-out begins.
    /// e.g. 0.001 ≈ −60 dB below peak.
    pub fade_level: f32,

    /// Duration of the fade-out ramp in seconds (linear, env → 0).
    pub fade_dur: f32,

    // ── Panning ───────────────────────────────────────────────────────────────

    /// Azimuth in radians.  Used by stereo and all ambisonic modes.
    pub azm: f32,

    /// Elevation in radians.  Used by 3D ambisonic modes.
    pub elev: f32,

    /// Distance.  Used for distance attenuation and reverb-channel modes.
    pub distance: f32,
}

impl FofParams {
    /// Returns the expected total duration (attack + decay until fade_level) in samples.
    ///
    /// Derived from:  exp(−α · t_end) = fade_level  →  t_end = −ln(fade_level) / α  (seconds)
    pub fn natural_duration_samples(&self, sample_rate: f32) -> f32 {
        if self.alpha <= 0.0 || self.fade_level <= 0.0 {
            return f32::MAX;
        }
        -self.fade_level.ln() / self.alpha * sample_rate
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Request to trigger an early fade-out on a tracked (id != 0) FOF.
#[derive(Clone, Copy, Debug)]
pub struct FofKillRequest {
    /// Must match the `id` in `FofParams`; ignored if not found.
    pub id: u64,

    /// Duration of the fade-out ramp in seconds.
    pub fade_dur: f32,
}

// ─────────────────────────────────────────────────────────────────────────────

/// Lifecycle phase of an active FOF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FofPhase {
    /// Sigmoid (half-cosine) attack: 0 ≤ t < β.
    Attack,

    /// Pure exponential decay: t ≥ β.
    Decay,

    /// Linear ramp to zero, triggered naturally or externally.
    FadeOut,

    /// Amplitude reached zero; slot returned to pool this block.
    Dead,
}

// ─────────────────────────────────────────────────────────────────────────────

use wide::f32x8;

use crate::fastsin::active_sin;

/// SIMD lane width used to synthesize each phase-uniform run of samples.
/// Matches this project's target (AVX2 = 256-bit = 8 x f32).
const LANES: usize = 8;

/// `[0.0, 1.0, .., 7.0]` — the per-lane sample offset within a chunk.
const IOTA: [f32; LANES] = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0];

/// Runtime state for one active FOF grain.
/// Lives in the slab allocator inside the audio thread.
pub struct FofState {
    /// Original parameters — kept for reference and natural fade-out check.
    pub params: FofParams,

    /// Current lifecycle phase.
    pub phase: FofPhase,

    /// Sine carrier phase accumulator, normalised to [0, 1).
    /// Recomputed once per `fill_block` call (not per sample) from the
    /// closed-form phase formula — kept for external/debug consumers.
    pub carrier_phase: f32,

    /// Instantaneous formant frequency (Hz).
    /// Recomputed once per `fill_block` call (not per sample) from the
    /// closed-form formula — kept for external/debug consumers.
    pub f_current: f32,

    /// Per-sample multiplicative glissando factor: 2^(gliss / sample_rate).
    /// == 1.0 when gliss == 0.0 (no glissando). `r` in the closed-form phase math.
    pub log_f_factor: f32,

    /// Samples elapsed since the start of the attack phase.
    pub t: u32,

    /// Envelope value captured at the moment fade-out was entered.
    pub env_at_fo: f32,

    /// `t` value at which fade-out was entered.
    pub t_fo: u32,

    /// Effective fade-out duration in samples (converted from seconds at spawn or kill time).
    pub effective_fade_dur: f32,

    /// Sample rate stored for converting kill-request fade_dur from seconds to samples.
    sample_rate: f32,

    // ── Closed-form / SIMD precomputed constants (set at spawn, O(1) cost) ──

    /// Carrier phase at t=0 (grain start), normalised to [0, 1).
    carrier_phase0: f32,

    /// params.f / sample_rate — per-sample phase increment with no glissando.
    f_over_sr: f32,

    /// 1 / (2 * beta_samples) — slope of the linear attack-rise phase ramp.
    /// 0.0 when beta_samples == 0 (attack is skipped entirely; see `attack_end`).
    inv_2beta: f32,

    /// Per-sample decay multiplier: exp(-alpha_per_sample). Combined with
    /// `d_pows` this reconstructs exp(-alpha_per_sample * t) for any t
    /// without a transcendental call per sample.
    d: f32,

    /// `[d^0, d^1, .., d^7]` — precomputed once per grain.
    d_pows: [f32; LANES],

    /// ln(r) == gliss * ln2 / sample_rate (r = log_f_factor). Kept around so
    /// `r^t - 1` can be computed via `expm1(t * ln_r)` — computing it as
    /// `r.powi(t) - 1.0` instead suffers catastrophic cancellation in f32
    /// once r^t gets close to 1 (i.e. for the great majority of practical
    /// gliss rates), losing 2+ decimal digits of precision.
    ln_r: f32,

    /// `[expm1(0*ln_r), expm1(1*ln_r), .., expm1(7*ln_r)]` == `[r^k - 1]`,
    /// each computed directly via `exp_m1` (not by subtracting 1 from r^k)
    /// — precomputed once per grain.
    r_pows_m1: [f32; LANES],

    /// `gliss_denom != 0.0` (gliss_denom == expm1(gliss * ln2 / sample_rate),
    /// computed via `exp_m1` to avoid catastrophic cancellation for small
    /// gliss rates — see `ln_r`'s doc comment), decided once at spawn.
    /// Selects between two separate compiled carrier-phase code paths (via
    /// the `GLISS` const generic on `carrier_phase_at`/`carrier_phase_chunk`/
    /// etc. and the `process_*_impl` functions) once per `process_*` call —
    /// not a branch re-evaluated per sample or per chunk.
    has_gliss: bool,

    /// (params.f / sample_rate) / gliss_denom — only valid when has_gliss.
    gliss_scale: f32,

    /// params.amp * amp_scale, folded into one constant.
    amp_total: f32,

    /// Sample index (grain-relative) at which Attack ends and Decay begins.
    attack_end: u32,

    /// Sample index (grain-relative) at which Decay's natural fade-level
    /// threshold is crossed and FadeOut begins. Always >= attack_end (the
    /// natural-fadeout check is only meaningful once Decay has begun, same
    /// as the phase-gated per-sample check this replaces). `u32::MAX` if
    /// alpha <= 0 or fade_level <= 0 (never triggers naturally).
    decay_end: u32,

    /// Sample index (grain-relative) at which FadeOut reaches zero and the
    /// grain dies. Set when entering FadeOut (naturally or via kill).
    fadeout_end: u32,
}

/// Returns the peak value of the FOF envelope as a function of α·β.
/// Used to normalise amplitude so `params.amp` is the true peak.
fn fof_amax(alpha: f32, beta: f32) -> f32 {
    let x = alpha * beta;
    if x < 0.01 {
        1.0
    } else if x <= 10.0 {
        let x = x.ln();
        let x2 = x * x;
        let a = -0.02677 * x2 * x - 0.2582 * x2 - 0.8238 * x - 0.9322;
        a.exp()
    } else {
        0.0 // pathological case — silence
    }
}

impl FofState {
    /// Promote a `FofParams` into an active `FofState`.
    /// Called when the time-wheel delivers the grain to the audio thread.
    pub fn spawn(params: FofParams, sample_rate: f32) -> Self {
        // f(t) = f * 2^(gliss * t / sample_rate)  →  per-sample factor = 2^(gliss / sample_rate)
        let log_f_factor = (params.gliss * std::f32::consts::LN_2 / sample_rate).exp();

        // Carrier phase initialised from phi (convert radians → [0,1))
        let carrier_phase0 = params.phi / std::f32::consts::TAU;
        let carrier_phase0 = carrier_phase0 - carrier_phase0.floor(); // wrap

        let beta_samples = params.beta * sample_rate;
        let alpha_per_sample = params.alpha / sample_rate;

        let peak = fof_amax(params.alpha, params.beta);
        let amp_scale = if peak > 0.0 { 1.0 / peak } else { 0.0 };

        let d = (-alpha_per_sample).exp();
        let mut d_pows = [1.0f32; LANES];
        for k in 1..LANES {
            d_pows[k] = d_pows[k - 1] * d;
        }

        // ln(r) == gliss * ln2 / sample_rate; gliss_denom == r - 1, via expm1
        // to stay accurate for small gliss rates (r very close to 1).
        let ln_r = params.gliss * std::f32::consts::LN_2 / sample_rate;
        let gliss_denom = ln_r.exp_m1();
        let mut r_pows_m1 = [0.0f32; LANES];
        for (k, slot) in r_pows_m1.iter_mut().enumerate() {
            *slot = (k as f32 * ln_r).exp_m1();
        }
        let f_over_sr = params.f / sample_rate;
        let gliss_scale = if gliss_denom != 0.0 { f_over_sr / gliss_denom } else { 0.0 };

        let inv_2beta = if beta_samples > 0.0 { 1.0 / (2.0 * beta_samples) } else { 0.0 };

        let attack_end = beta_samples.max(0.0).ceil() as u32;
        let decay_end_raw = if params.alpha > 0.0 && params.fade_level > 0.0 {
            (-params.fade_level.ln() / alpha_per_sample).max(0.0).ceil() as u32
        } else {
            u32::MAX
        };
        // The natural fade-level check only ever applied once Decay had
        // begun in the original per-sample state machine — never during
        // Attack — so clamp to attack_end to preserve that gating exactly.
        let decay_end = decay_end_raw.max(attack_end);

        FofState {
            params,
            phase: FofPhase::Attack,
            carrier_phase: carrier_phase0,
            f_current: params.f,
            log_f_factor,
            t: 0,
            env_at_fo: 0.0,
            t_fo: 0,
            effective_fade_dur: params.fade_dur * sample_rate,
            sample_rate,
            carrier_phase0,
            f_over_sr,
            inv_2beta,
            d,
            d_pows,
            ln_r,
            r_pows_m1,
            has_gliss: gliss_denom != 0.0,
            gliss_scale,
            amp_total: params.amp * amp_scale,
            attack_end,
            decay_end,
            fadeout_end: u32::MAX,
        }
    }

    /// Current envelope value (0..amp-normalised peak) at `self.t`, given
    /// `self.phase`. O(1), but not vectorized — only called at phase-transition
    /// boundaries (a handful of times per grain lifetime), never per sample.
    pub fn envelope(&self) -> f32 {
        match self.phase {
            FofPhase::Attack => self.attack_rise_at(self.t) * self.decay_env_at(self.t),
            FofPhase::Decay => self.decay_env_at(self.t),
            FofPhase::FadeOut => {
                let elapsed = (self.t - self.t_fo) as f32;
                let frac = (1.0 - elapsed / self.effective_fade_dur).max(0.0);
                self.env_at_fo * frac
            }
            FofPhase::Dead => 0.0,
        }
    }

    /// Fill `buf` with mono samples for this block, advancing all accumulators.
    ///
    /// `buf` must be pre-zeroed; `block_start` is the absolute sample index of
    /// `buf[0]`.  Sub-block start and mid-block Dead transitions are handled
    /// internally — samples before the grain's start and after its death stay 0.
    pub fn fill_block(&mut self, _sample_rate: f32, block_start: u64, buf: &mut [f32]) {
        if self.phase == FofPhase::Dead {
            return;
        }

        let start_offset: usize = if self.params.start_sample > block_start {
            (self.params.start_sample - block_start) as usize
        } else {
            0
        };

        let block_size = buf.len();
        debug_assert!(start_offset < block_size);

        let mut i = start_offset;
        while i < block_size {
            let phase_end = match self.phase {
                FofPhase::Attack => self.attack_end,
                FofPhase::Decay => self.decay_end,
                FofPhase::FadeOut => self.fadeout_end,
                FofPhase::Dead => break,
            };

            let remaining = phase_end.saturating_sub(self.t);
            if remaining == 0 {
                self.transition_phase();
                continue;
            }

            let run_len = (remaining as usize).min(block_size - i);
            match self.phase {
                FofPhase::Attack => self.process_attack(&mut buf[i..i + run_len]),
                FofPhase::Decay => self.process_decay(&mut buf[i..i + run_len]),
                FofPhase::FadeOut => self.process_fadeout(&mut buf[i..i + run_len]),
                FofPhase::Dead => unreachable!(),
            }
            self.t += run_len as u32;
            i += run_len;
        }

        // Keep the public debug/reference fields consistent — O(1) per block.
        let phase = if self.has_gliss {
            self.carrier_phase_at::<true>(self.t)
        } else {
            self.carrier_phase_at::<false>(self.t)
        };
        self.carrier_phase = wrap_phase(phase);
        self.f_current = self.params.f * self.log_f_factor.powi(self.t as i32);
    }

    /// Trigger an external fade-out (from a kill request).
    /// No-op if already in FadeOut or Dead.
    pub fn trigger_fade_out(&mut self, fade_dur: f32) {
        if matches!(self.phase, FofPhase::FadeOut | FofPhase::Dead) {
            return;
        }
        self.env_at_fo = self.envelope();
        self.t_fo = self.t;
        self.effective_fade_dur = fade_dur * self.sample_rate;
        self.fadeout_end = self.t_fo + self.effective_fade_dur.max(0.0).ceil() as u32;
        self.phase = FofPhase::FadeOut;
    }

    // ── Private: phase transitions ──────────────────────────────────────────

    /// Advance to the next lifecycle phase; called when a phase's sample
    /// budget (`attack_end` / `decay_end` / `fadeout_end`) has been reached.
    fn transition_phase(&mut self) {
        match self.phase {
            FofPhase::Attack => {
                self.phase = FofPhase::Decay;
            }
            FofPhase::Decay => {
                self.env_at_fo = self.decay_env_at(self.t);
                self.t_fo = self.t;
                self.effective_fade_dur = self.params.fade_dur * self.sample_rate;
                self.fadeout_end = self.t_fo + self.effective_fade_dur.max(0.0).ceil() as u32;
                self.phase = FofPhase::FadeOut;
            }
            FofPhase::FadeOut => {
                self.phase = FofPhase::Dead;
            }
            FofPhase::Dead => {}
        }
    }

    // ── Private: closed-form scalar formulas (single source of truth,
    // shared between the SIMD chunk builders and their scalar remainder
    // loops / envelope()) ─────────────────────────────────────────────────

    /// exp(-alpha_per_sample * t), evaluated fresh from t (no drift).
    #[inline]
    fn decay_env_at(&self, t: u32) -> f32 {
        self.d.powi(t as i32)
    }

    /// 0.5*(1 - cos(pi*t/beta_samples)), evaluated fresh from t via the LUT.
    #[inline]
    fn attack_rise_at(&self, t: u32) -> f32 {
        let phase = t as f32 * self.inv_2beta + 0.25; // cos(x) == active_sin(x/TAU + 0.25)
        0.5 * (1.0 - active_sin(phase))
    }

    /// Carrier phase (cycles, unwrapped) at grain-relative sample `t`.
    ///
    /// `GLISS` picks between two entirely separate compiled paths: a plain
    /// linear phase ramp (no glissando) and the geometric-series closed form
    /// (glissando). This is decided once per grain (`self.has_gliss`, see its
    /// doc comment) by the `process_*` dispatchers below — never a per-sample
    /// or per-chunk branch.
    #[inline]
    fn carrier_phase_at<const GLISS: bool>(&self, t: u32) -> f32 {
        if GLISS {
            // r^t - 1 via expm1 directly — avoids the catastrophic
            // cancellation of computing r.powi(t) and then subtracting 1.
            let r_pow_m1 = (t as f32 * self.ln_r).exp_m1();
            self.carrier_phase0 + self.gliss_scale * r_pow_m1
        } else {
            self.carrier_phase0 + self.f_over_sr * t as f32
        }
    }

    #[inline]
    fn carrier_sine_at<const GLISS: bool>(&self, t: u32) -> f32 {
        active_sin(self.carrier_phase_at::<GLISS>(t))
    }

    // ── Private: SIMD (8-lane) chunk builders ───────────────────────────────
    //
    // Each returns a `f32x8` for samples [t0, t0+8). The sine LUT gather
    // itself is inherently scalar (no unsafe AVX2 gather intrinsics), so
    // these compute the phase vector via SIMD arithmetic, then do 8 scalar
    // `active_sin` lookups — everything else (phase/envelope math, the final
    // multiply-accumulate into `buf`) is vectorized. `GLISS` — see
    // `carrier_phase_at`'s doc comment — gives each of the two cases (plain
    // linear ramp vs. geometric-series closed form) its own compiled body
    // with no runtime branch inside the chunk loop.

    #[inline]
    fn carrier_phase_chunk<const GLISS: bool>(&self, t0: u32) -> f32x8 {
        if GLISS {
            // r^(t0+lane) - 1, split as (1+a)(1+b)-1 = a+b+a*b with
            // a = r^t0-1, b = r^lane-1 both obtained directly via expm1 —
            // avoids computing r^(t0+lane) (close to 1) and subtracting 1,
            // which loses precision catastrophically in f32 (see `ln_r` doc).
            let r_base_m1 = (t0 as f32 * self.ln_r).exp_m1();
            let a = f32x8::splat(r_base_m1);
            let b = f32x8::from(self.r_pows_m1);
            let numerator = a + b + a * b;
            f32x8::splat(self.carrier_phase0) + numerator * f32x8::splat(self.gliss_scale)
        } else {
            let base = self.carrier_phase0 + self.f_over_sr * t0 as f32;
            f32x8::splat(base) + f32x8::from(IOTA) * f32x8::splat(self.f_over_sr)
        }
    }

    #[inline]
    fn carrier_sine_chunk<const GLISS: bool>(&self, t0: u32) -> f32x8 {
        let phase_arr = self.carrier_phase_chunk::<GLISS>(t0).to_array();
        let mut sine_arr = [0.0f32; LANES];
        for lane in 0..LANES {
            sine_arr[lane] = active_sin(phase_arr[lane]);
        }
        f32x8::from(sine_arr)
    }

    #[inline]
    fn decay_env_chunk(&self, t0: u32) -> f32x8 {
        let d_base = self.d.powi(t0 as i32);
        f32x8::splat(d_base) * f32x8::from(self.d_pows)
    }

    #[inline]
    fn attack_rise_chunk(&self, t0: u32) -> f32x8 {
        let base = t0 as f32 * self.inv_2beta + 0.25;
        let phase_vec = f32x8::splat(base) + f32x8::from(IOTA) * f32x8::splat(self.inv_2beta);
        let phase_arr = phase_vec.to_array();
        let mut cos_arr = [0.0f32; LANES];
        for lane in 0..LANES {
            cos_arr[lane] = active_sin(phase_arr[lane]);
        }
        (f32x8::splat(1.0) - f32x8::from(cos_arr)) * f32x8::splat(0.5)
    }

    // ── Private: per-phase block processors ─────────────────────────────────
    //
    // Each processes a phase-uniform run of `buf`, SIMD_WIDTH samples at a
    // time, with a scalar tail for the `< LANES`-sample remainder. `buf` is
    // the mono scratch slice for exactly this run (already sliced by the
    // caller to `[i..i+run_len]`), and `self.t` is the absolute grain-relative
    // sample index of `buf[0]`.
    //
    // Each has a thin non-generic dispatcher (`process_attack` etc.) plus a
    // `<const GLISS: bool>` implementation — chosen once per call via
    // `self.has_gliss`, giving the glissando and non-glissando cases their
    // own separately-compiled loop bodies (see `carrier_phase_at`'s doc
    // comment) instead of re-checking a per-grain-constant condition inside
    // every SIMD chunk.

    fn process_attack(&mut self, buf: &mut [f32]) {
        if self.has_gliss {
            self.process_attack_impl::<true>(buf);
        } else {
            self.process_attack_impl::<false>(buf);
        }
    }

    // `inline(never)`: each of the two monomorphizations has exactly one
    // call site (in `process_attack`/`process_decay`/`process_fadeout`
    // below), so without this the compiler happily inlines *both* GLISS=true
    // and GLISS=false bodies into one bloated function — doubling its size
    // even though a given grain only ever executes one side. That hurt
    // (measured ~6-10% regression on a decay-heavy benchmark) rather than
    // helped, since the runtime branch it replaced was already
    // near-perfectly predicted for any single grain's lifetime. Keeping
    // these as genuinely separate out-of-line functions gives each GLISS
    // path its own compact, cache-friendly code instead.
    #[inline(never)]
    fn process_attack_impl<const GLISS: bool>(&mut self, buf: &mut [f32]) {
        let n = buf.len();
        let simd_end = n - n % LANES;
        let amp_total = f32x8::splat(self.amp_total);

        let mut idx = 0;
        while idx < simd_end {
            let t0 = self.t + idx as u32;
            let env_vec = self.decay_env_chunk(t0) * self.attack_rise_chunk(t0);
            let sine_vec = self.carrier_sine_chunk::<GLISS>(t0);
            let buf_vec = f32x8::from(<[f32; LANES]>::try_from(&buf[idx..idx + LANES]).unwrap());
            let out = (env_vec * sine_vec).mul_add(amp_total, buf_vec);
            buf[idx..idx + LANES].copy_from_slice(&out.to_array());
            idx += LANES;
        }
        while idx < n {
            let t = self.t + idx as u32;
            let env = self.decay_env_at(t) * self.attack_rise_at(t);
            buf[idx] += self.amp_total * env * self.carrier_sine_at::<GLISS>(t);
            idx += 1;
        }
    }

    fn process_decay(&mut self, buf: &mut [f32]) {
        if self.has_gliss {
            self.process_decay_impl::<true>(buf);
        } else {
            self.process_decay_impl::<false>(buf);
        }
    }

    #[inline(never)] // see process_attack_impl's doc comment
    fn process_decay_impl<const GLISS: bool>(&mut self, buf: &mut [f32]) {
        let n = buf.len();
        let simd_end = n - n % LANES;
        let amp_total = f32x8::splat(self.amp_total);

        let mut idx = 0;
        while idx < simd_end {
            let t0 = self.t + idx as u32;
            let env_vec = self.decay_env_chunk(t0);
            let sine_vec = self.carrier_sine_chunk::<GLISS>(t0);
            let buf_vec = f32x8::from(<[f32; LANES]>::try_from(&buf[idx..idx + LANES]).unwrap());
            let out = (env_vec * sine_vec).mul_add(amp_total, buf_vec);
            buf[idx..idx + LANES].copy_from_slice(&out.to_array());
            idx += LANES;
        }
        while idx < n {
            let t = self.t + idx as u32;
            let env = self.decay_env_at(t);
            buf[idx] += self.amp_total * env * self.carrier_sine_at::<GLISS>(t);
            idx += 1;
        }
    }

    fn process_fadeout(&mut self, buf: &mut [f32]) {
        if self.has_gliss {
            self.process_fadeout_impl::<true>(buf);
        } else {
            self.process_fadeout_impl::<false>(buf);
        }
    }

    #[inline(never)] // see process_attack_impl's doc comment
    fn process_fadeout_impl<const GLISS: bool>(&mut self, buf: &mut [f32]) {
        let n = buf.len();
        let simd_end = n - n % LANES;
        let amp_total = f32x8::splat(self.amp_total);
        let env_at_fo = f32x8::splat(self.env_at_fo);
        let inv_dur = if self.effective_fade_dur > 0.0 { 1.0 / self.effective_fade_dur } else { 0.0 };
        let t_fo = self.t_fo;

        let mut idx = 0;
        while idx < simd_end {
            let t0 = self.t + idx as u32;
            let elapsed_base = (t0 - t_fo) as f32;
            let elapsed_vec = f32x8::splat(elapsed_base) + f32x8::from(IOTA);
            let frac = (f32x8::splat(1.0) - elapsed_vec * f32x8::splat(inv_dur)).max(f32x8::splat(0.0));
            let env_vec = env_at_fo * frac;
            let sine_vec = self.carrier_sine_chunk::<GLISS>(t0);
            let buf_vec = f32x8::from(<[f32; LANES]>::try_from(&buf[idx..idx + LANES]).unwrap());
            let out = (env_vec * sine_vec).mul_add(amp_total, buf_vec);
            buf[idx..idx + LANES].copy_from_slice(&out.to_array());
            idx += LANES;
        }
        while idx < n {
            let t = self.t + idx as u32;
            let elapsed = (t - t_fo) as f32;
            let frac = (1.0 - elapsed * inv_dur).max(0.0);
            let env = self.env_at_fo * frac;
            buf[idx] += self.amp_total * env * self.carrier_sine_at::<GLISS>(t);
            idx += 1;
        }
    }
}

#[inline]
fn wrap_phase(phase: f32) -> f32 {
    phase - phase.floor()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    fn standard_params() -> FofParams {
        FofParams {
            id: 0,
            start_sample: 0,
            f: 440.0,
            gliss: 0.0,
            phi: 0.0,
            amp: 1.0,
            alpha: 20.0,
            beta: 0.01,
            fade_level: 0.001,
            fade_dur: 0.01,
            azm: 0.0,
            elev: 0.0,
            distance: 1.0,
        }
    }

    /// Naive per-sample reference implementation mirroring the original
    /// (pre-SIMD) `fill_block` exactly: real `.sin()`/`.cos()`/`.exp()` per
    /// sample, one sample at a time, no closed-form shortcuts.
    struct NaiveRef {
        params: FofParams,
        phase: FofPhase,
        carrier_phase: f32,
        f_current: f32,
        log_f_factor: f32,
        t: u32,
        env_at_fo: f32,
        t_fo: u32,
        effective_fade_dur: f32,
        sample_rate: f32,
        amp_scale: f32,
        beta_samples: f32,
        alpha_per_sample: f32,
    }

    impl NaiveRef {
        fn spawn(params: FofParams, sample_rate: f32) -> Self {
            let log_f_factor = (params.gliss * std::f32::consts::LN_2 / sample_rate).exp();
            let carrier_phase = params.phi / std::f32::consts::TAU;
            let carrier_phase = carrier_phase - carrier_phase.floor();
            let beta_samples = params.beta * sample_rate;
            let alpha_per_sample = params.alpha / sample_rate;
            let peak = fof_amax(params.alpha, params.beta);
            let amp_scale = if peak > 0.0 { 1.0 / peak } else { 0.0 };
            NaiveRef {
                params,
                phase: FofPhase::Attack,
                carrier_phase,
                f_current: params.f,
                log_f_factor,
                t: 0,
                env_at_fo: 0.0,
                t_fo: 0,
                effective_fade_dur: params.fade_dur * sample_rate,
                sample_rate,
                amp_scale,
                beta_samples,
                alpha_per_sample,
            }
        }

        fn envelope(&self) -> f32 {
            let t = self.t as f32;
            match self.phase {
                FofPhase::Attack => {
                    let rise = 0.5 * (1.0 - (std::f32::consts::PI * t / self.beta_samples).cos());
                    rise * (-self.alpha_per_sample * t).exp()
                }
                FofPhase::Decay => (-self.alpha_per_sample * t).exp(),
                FofPhase::FadeOut => {
                    let elapsed = (self.t - self.t_fo) as f32;
                    let dur = self.effective_fade_dur;
                    let frac = (1.0 - elapsed / dur).max(0.0);
                    self.env_at_fo * frac
                }
                FofPhase::Dead => 0.0,
            }
        }

        fn fill_block(&mut self, sample_rate: f32, block_start: u64, buf: &mut [f32]) {
            if self.phase == FofPhase::Dead {
                return;
            }
            let start_offset: usize = if self.params.start_sample > block_start {
                (self.params.start_sample - block_start) as usize
            } else {
                0
            };
            let block_size = buf.len();
            for i in start_offset..block_size {
                if self.phase == FofPhase::Dead {
                    break;
                }
                let env = self.envelope();
                let sine = (self.carrier_phase * std::f32::consts::TAU).sin();
                buf[i] += self.params.amp * self.amp_scale * env * sine;

                self.carrier_phase += self.f_current / sample_rate;
                self.carrier_phase -= self.carrier_phase.floor();
                self.f_current *= self.log_f_factor;
                self.t += 1;
                self.update_phase();
            }
        }

        fn trigger_fade_out(&mut self, fade_dur: f32) {
            if matches!(self.phase, FofPhase::FadeOut | FofPhase::Dead) {
                return;
            }
            self.env_at_fo = self.envelope();
            self.t_fo = self.t;
            self.effective_fade_dur = fade_dur * self.sample_rate;
            self.phase = FofPhase::FadeOut;
        }

        fn update_phase(&mut self) {
            match self.phase {
                FofPhase::Attack => {
                    if self.t as f32 >= self.beta_samples {
                        self.phase = FofPhase::Decay;
                    }
                }
                FofPhase::Decay => {
                    let env = (-self.alpha_per_sample * self.t as f32).exp();
                    if env < self.params.fade_level {
                        self.env_at_fo = env;
                        self.t_fo = self.t;
                        self.effective_fade_dur = self.params.fade_dur * self.sample_rate;
                        self.phase = FofPhase::FadeOut;
                    }
                }
                FofPhase::FadeOut => {
                    if (self.t - self.t_fo) as f32 >= self.effective_fade_dur {
                        self.phase = FofPhase::Dead;
                    }
                }
                FofPhase::Dead => {}
            }
        }
    }

    // LUT quantization bound, same derivation as fastsin.rs's own test.
    fn lut_bound() -> f32 {
        // TABLE_BITS = 12 -> 4096 entries (see build.rs); mirror that bound
        // here rather than reaching into the generated table internals.
        std::f32::consts::PI / 4096.0 * 1.5
    }

    /// Runs both implementations for `n_blocks` of `block_size` samples,
    /// starting at `start_sample`, asserting per-sample agreement within a
    /// tolerance that accounts for LUT quantization and f32 accumulation.
    fn assert_matches_naive(params: FofParams, n_blocks: usize, block_size: usize, kill_at_block: Option<(usize, f32)>) {
        let mut fast = FofState::spawn(params, SR);
        let mut naive = NaiveRef::spawn(params, SR);

        // Tolerance: worst-case single-sample LUT error, amplified a bit for
        // envelope-slope interaction (env * d(sine)/d(phase) term) and f32
        // rounding in the closed-form vs. per-sample-recurrence formulas.
        let tol = lut_bound() * 4.0 + 1e-4;

        for block in 0..n_blocks {
            let block_start = (block * block_size) as u64;
            let mut fast_buf = vec![0.0f32; block_size];
            let mut naive_buf = vec![0.0f32; block_size];

            fast.fill_block(SR, block_start, &mut fast_buf);
            naive.fill_block(SR, block_start, &mut naive_buf);

            if let Some((kill_block, fade_dur)) = kill_at_block
                && block == kill_block
            {
                fast.trigger_fade_out(fade_dur);
                naive.trigger_fade_out(fade_dur);
            }

            for (i, (&f, &n)) in fast_buf.iter().zip(naive_buf.iter()).enumerate() {
                assert!(f.is_finite(), "block {block} sample {i}: fast output not finite ({f})");
                assert!(
                    (f - n).abs() < tol,
                    "block {block} sample {i}: fast={f} naive={n} diff={} tol={tol}",
                    (f - n).abs()
                );
            }
        }
    }

    #[test]
    fn attack_decay_fadeout_natural_progression() {
        let params = standard_params();
        // natural_duration_samples ~= 16.5k @ 48kHz; run well past death.
        assert_matches_naive(params, 40, 512, None);
    }

    #[test]
    fn kill_triggered_fade_out_mid_decay() {
        let params = standard_params();
        // Block 2 (samples 1024..1536) is well past the ~480-sample attack,
        // solidly inside Decay.
        assert_matches_naive(params, 40, 512, Some((2, 0.05)));
    }

    #[test]
    fn kill_triggered_fade_out_during_attack() {
        let mut params = standard_params();
        params.beta = 0.5; // long attack (24k samples) so a kill lands inside it
        assert_matches_naive(params, 10, 512, Some((0, 0.02)));
    }

    #[test]
    fn no_glissando_matches_naive() {
        let mut params = standard_params();
        params.gliss = 0.0;
        assert_matches_naive(params, 20, 512, None);
    }

    #[test]
    fn positive_glissando_matches_naive() {
        let mut params = standard_params();
        params.gliss = 3.0; // 3 oct/sec upward
        // Short-ish span: the naive reference is a per-sample f32 recurrence
        // that itself accumulates phase drift over long durations (unlike
        // the closed form here, recomputed fresh from t every chunk — see
        // `ln_r`'s doc comment). This still covers Attack (480 samples) plus
        // a solid stretch of Decay, which is enough to catch a formula bug.
        assert_matches_naive(params, 5, 512, None);
    }

    #[test]
    fn negative_glissando_matches_naive() {
        let mut params = standard_params();
        params.gliss = -2.5;
        assert_matches_naive(params, 5, 512, None);
    }

    #[test]
    fn small_glissando_avoids_cancellation() {
        // Small gliss rates make (r - 1) tiny (~1e-5 at 48kHz) — this is
        // exactly the case expm1 is needed for; a naive `r - 1.0` would lose
        // precision here.
        let mut params = standard_params();
        params.gliss = 1.0;
        params.alpha = 2.0; // slower decay, but keep the span short (see above)
        assert_matches_naive(params, 5, 512, None);
    }

    #[test]
    fn sub_block_start_offset_matches_naive() {
        let mut params = standard_params();
        params.start_sample = 137; // mid-block start within block 0 (size 512)
        assert_matches_naive(params, 20, 512, None);
    }

    #[test]
    fn multi_block_continuity_matches_naive() {
        // Small block size relative to grain duration forces many
        // `fill_block` calls across the grain's lifetime, exercising
        // cross-call state continuity (t, phase transitions mid-stream).
        let params = standard_params();
        assert_matches_naive(params, 300, 64, None);
    }

    #[test]
    fn zero_beta_no_nan_and_terminates() {
        let mut params = standard_params();
        params.beta = 0.0;
        let mut fast = FofState::spawn(params, SR);
        for block in 0..40u64 {
            let mut buf = vec![0.0f32; 512];
            fast.fill_block(SR, block * 512, &mut buf);
            for (i, &s) in buf.iter().enumerate() {
                assert!(s.is_finite(), "block {block} sample {i}: non-finite ({s})");
            }
        }
        assert_eq!(fast.phase, FofPhase::Dead);
    }

    #[test]
    fn zero_fade_dur_kill_dies_immediately() {
        let params = standard_params();
        let mut fast = FofState::spawn(params, SR);
        let mut buf = vec![0.0f32; 512];
        fast.fill_block(SR, 0, &mut buf);
        fast.trigger_fade_out(0.0);
        let mut buf2 = vec![0.0f32; 512];
        fast.fill_block(SR, 512, &mut buf2);
        assert_eq!(fast.phase, FofPhase::Dead);
    }

    #[test]
    fn dies_eventually_and_stays_silent() {
        let params = standard_params();
        let mut fast = FofState::spawn(params, SR);
        let mut block_start = 0u64;
        loop {
            let mut buf = vec![0.0f32; 512];
            fast.fill_block(SR, block_start, &mut buf);
            block_start += 512;
            if fast.phase == FofPhase::Dead {
                break;
            }
            assert!(block_start < 10 * SR as u64, "FOF did not die within 10s");
        }
        // A further block after death must stay silent (fill_block early-returns).
        let mut buf = vec![0.0f32; 512];
        fast.fill_block(SR, block_start, &mut buf);
        assert!(buf.iter().all(|&s| s == 0.0));
    }
}
