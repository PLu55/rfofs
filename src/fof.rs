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

    /// Formant frequency at grain end (Hz). Equal to `f` if no glissando.
    /// Glissando is exponential (linear in octaves/sec).
    pub f_end: f32,

    /// Initial sine phase (radians).
    pub phi: f32,

    /// Peak amplitude (envelope maximum).
    pub amp: f32,

    // ── Envelope ─────────────────────────────────────────────────────────────

    /// Exponential decay coefficient α.  env_decay(t) = exp(−α·t).
    pub alpha: f32,

    /// Attack duration β in samples (half-cosine sigmoid).
    pub beta: f32,

    // ── Natural fade-out ─────────────────────────────────────────────────────

    /// Threshold relative to `amp` at which the natural fade-out begins.
    /// e.g. 0.001 ≈ −60 dB below peak.
    pub fade_level: f32,

    /// Duration of the fade-out ramp in samples (linear, env → 0).
    pub fade_dur: u32,

    // ── Panning ───────────────────────────────────────────────────────────────

    /// Azimuth in radians.  Used by stereo and all ambisonic modes.
    pub azm: f32,

    /// Elevation in radians.  Used by 3D ambisonic modes.
    pub elev: f32,

    /// Distance.  Used for distance attenuation and reverb-channel modes.
    pub distance: f32,
}

impl FofParams {
    /// Returns the expected total duration (attack + decay until fade_level)
    /// in fractional samples.  Used to precompute the glissando rate.
    ///
    /// Derived from:  exp(−α · t_end) = fade_level  →  t_end = −ln(fade_level) / α
    pub fn natural_duration_samples(&self) -> f32 {
        if self.alpha <= 0.0 || self.fade_level <= 0.0 {
            return f32::MAX;
        }
        -self.fade_level.ln() / self.alpha
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Request to trigger an early fade-out on a tracked (id != 0) FOF.
#[derive(Clone, Copy, Debug)]
pub struct FofKillRequest {
    /// Must match the `id` in `FofParams`; ignored if not found.
    pub id: u64,

    /// Duration of the fade-out ramp in samples.
    pub fade_dur: u32,
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

/// Runtime state for one active FOF grain.
/// Lives in the slab allocator inside the audio thread.
pub struct FofState {
    /// Original parameters — kept for reference and natural fade-out check.
    pub params: FofParams,

    /// Current lifecycle phase.
    pub phase: FofPhase,

    /// Sine carrier phase accumulator, normalised to [0, 1).
    pub carrier_phase: f32,

    /// Instantaneous formant frequency (Hz), updated each sample for glissando.
    pub f_current: f32,

    /// Per-sample multiplicative glissando factor: exp(ln(f_end/f) / duration).
    /// == 1.0 when f_end == f (no glissando).
    pub log_f_factor: f32,

    /// Samples elapsed since the start of the attack phase.
    pub t: u32,

    /// Envelope value captured at the moment fade-out was entered.
    pub env_at_fo: f32,

    /// `t` value at which fade-out was entered.
    pub t_fo: u32,

    /// Effective fade-out duration in samples (from params or kill request).
    pub effective_fade_dur: u32,
}

impl FofState {
    /// Promote a `FofParams` into an active `FofState`.
    /// Called when the time-wheel delivers the grain to the audio thread.
    pub fn spawn(params: FofParams, sample_rate: f32) -> Self {
        let duration = params.natural_duration_samples();

        let log_f_factor = if (params.f_end - params.f).abs() < 1e-6 || duration == f32::MAX {
            1.0_f32
        } else {
            // f(t) = f_start · factor^t  →  factor = (f_end/f_start)^(1/duration)
            (params.f_end / params.f).ln().div_euclid(duration).exp()
        };

        // Carrier phase initialised from phi (convert radians → [0,1))
        let carrier_phase = params.phi / std::f32::consts::TAU;
        let carrier_phase = carrier_phase - carrier_phase.floor(); // wrap

        FofState {
            params,
            phase: FofPhase::Attack,
            carrier_phase,
            f_current: params.f,
            log_f_factor,
            t: 0,
            env_at_fo: 0.0,
            t_fo: 0,
            effective_fade_dur: params.fade_dur,
        }
    }

    /// Compute the envelope value for the current sample.
    #[inline]
    pub fn envelope(&self) -> f32 {
        let t = self.t as f32;
        match self.phase {
            FofPhase::Attack => {
                let beta = self.params.beta;
                let rise = 0.5 * (1.0 - (std::f32::consts::PI * t / beta).cos());
                rise * (-self.params.alpha * t).exp()
            }
            FofPhase::Decay => {
                (-self.params.alpha * t).exp()
            }
            FofPhase::FadeOut => {
                let elapsed = (self.t - self.t_fo) as f32;
                let dur = self.effective_fade_dur as f32;
                let frac = (1.0 - elapsed / dur).max(0.0);
                self.env_at_fo * frac
            }
            FofPhase::Dead => 0.0,
        }
    }

    /// Compute one output sample and advance all accumulators.
    /// Returns the unscaled mono sample (before panning).
    #[inline]
    pub fn next_sample(&mut self, sample_rate: f32) -> f32 {
        if self.phase == FofPhase::Dead {
            return 0.0;
        }

        let env = self.envelope();
        let sine = (self.carrier_phase * std::f32::consts::TAU).sin();
        let out = self.params.amp * env * sine;

        // ── Advance carrier ───────────────────────────────────────────────
        self.carrier_phase += self.f_current / sample_rate;
        self.carrier_phase -= self.carrier_phase.floor(); // wrap [0,1)
        self.f_current *= self.log_f_factor;              // glissando step

        // ── Advance time and update phase ─────────────────────────────────
        self.t += 1;
        self.update_phase();

        out
    }

    /// Trigger an external fade-out (from a kill request).
    /// No-op if already in FadeOut or Dead.
    pub fn trigger_fade_out(&mut self, fade_dur: u32) {
        if matches!(self.phase, FofPhase::FadeOut | FofPhase::Dead) {
            return;
        }
        self.env_at_fo = self.envelope();
        self.t_fo = self.t;
        self.effective_fade_dur = fade_dur;
        self.phase = FofPhase::FadeOut;
    }

    /// Advance the phase state machine after each sample.
    #[inline]
    fn update_phase(&mut self) {
        match self.phase {
            FofPhase::Attack => {
                if self.t as f32 >= self.params.beta {
                    self.phase = FofPhase::Decay;
                }
            }
            FofPhase::Decay => {
                // Check natural fade-out threshold
                let env = (-self.params.alpha * self.t as f32).exp();
                if env < self.params.fade_level {
                    self.env_at_fo = env;
                    self.t_fo = self.t;
                    self.effective_fade_dur = self.params.fade_dur;
                    self.phase = FofPhase::FadeOut;
                }
            }
            FofPhase::FadeOut => {
                let elapsed = self.t - self.t_fo;
                if elapsed >= self.effective_fade_dur {
                    self.phase = FofPhase::Dead;
                }
            }
            FofPhase::Dead => {}
        }
    }
}
