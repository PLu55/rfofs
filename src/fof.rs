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

    /// Per-sample multiplicative glissando factor: 2^(gliss / sample_rate).
    /// == 1.0 when gliss == 0.0 (no glissando).
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

    /// Reciprocal of fof_amax(alpha, beta): multiplied into amp so the true
    /// output peak equals params.amp regardless of alpha/beta.
    amp_scale: f32,

    /// Attack duration in samples: params.beta * sample_rate.
    beta_samples: f32,

    /// Per-sample decay coefficient: params.alpha / sample_rate.
    alpha_per_sample: f32,
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
        let carrier_phase = params.phi / std::f32::consts::TAU;
        let carrier_phase = carrier_phase - carrier_phase.floor(); // wrap

        let beta_samples = params.beta * sample_rate;
        let alpha_per_sample = params.alpha / sample_rate;

        let peak = fof_amax(params.alpha, params.beta);
        let amp_scale = if peak > 0.0 { 1.0 / peak } else { 0.0 };

        FofState {
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

    /// Compute the envelope value for the current sample.
    #[inline]
    pub fn envelope(&self) -> f32 {
        let t = self.t as f32;
        match self.phase {
            FofPhase::Attack => {
                let rise = 0.5 * (1.0 - (std::f32::consts::PI * t / self.beta_samples).cos());
                rise * (-self.alpha_per_sample * t).exp()
            }
            FofPhase::Decay => {
                (-self.alpha_per_sample * t).exp()
            }
            FofPhase::FadeOut => {
                let elapsed = (self.t - self.t_fo) as f32;
                let dur = self.effective_fade_dur;
                let frac = (1.0 - elapsed / dur).max(0.0);
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
    pub fn fill_block(&mut self, sample_rate: f32, block_start: u64, buf: &mut [f32]) {
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

        for i in start_offset..block_size {
            if self.phase == FofPhase::Dead {
                break;
            }

            let env  = self.envelope();
            let sine = (self.carrier_phase * std::f32::consts::TAU).sin();
            buf[i]  += self.params.amp * self.amp_scale * env * sine;

            self.carrier_phase += self.f_current / sample_rate;
            self.carrier_phase -= self.carrier_phase.floor();
            self.f_current     *= self.log_f_factor;
            self.t += 1;
            self.update_phase();
        }
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
        self.phase = FofPhase::FadeOut;
    }

    /// Advance the phase state machine after each sample.
    #[inline]
    fn update_phase(&mut self) {
        match self.phase {
            FofPhase::Attack => {
                if self.t as f32 >= self.beta_samples {
                    self.phase = FofPhase::Decay;
                }
            }
            FofPhase::Decay => {
                // Check natural fade-out threshold
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
