use std::collections::HashMap;
use slab::Slab;

use crate::fof::{FofKillRequest, FofParams, FofPhase, FofState};
use crate::pan::{pan_gains, PanMode};
use crate::queue::{KillQueueConsumer, TimeWheelConsumer};

// ─────────────────────────────────────────────────────────────────────────────

/// The core audio-thread engine.
///
/// # Ownership model
/// `RfofsEngine` is owned exclusively by the JACK/Pipewire process callback.
/// It must never be shared across threads; all cross-thread communication
/// goes through the lock-free queues.
pub struct RfofsEngine {
    /// Sample rate (Hz).
    sample_rate: f32,

    /// Global panning mode — set at startup, uniform for all FOFs.
    pan_mode: PanMode,

    /// Active FOF pool.  Slab gives O(1) insert/remove with stable indices.
    active: Slab<FofState>,

    /// id → slab index map.  Only contains entries where id != 0.
    id_map: HashMap<u64, usize>,

    /// Scratch buffer for params drained from the time-wheel this block.
    incoming: Vec<FofParams>,

    /// Scratch buffer for kill requests drained this block.
    kill_requests: Vec<FofKillRequest>,

    /// Mono synthesis scratch (length = max_block_size). Reused across FOFs.
    mono_buf: Vec<f32>,

    /// Per-channel pan gain scratch (length = pan_mode.channel_count()).
    gains_buf: Vec<f32>,

    /// Reusable slab-key list — avoids per-block Vec allocation.
    active_keys: Vec<usize>,

    /// Reusable dead-FOF collection buffer.
    dead_keys: Vec<(usize, u64)>,

    /// Absolute sample counter — incremented by block_size each block.
    sample_clock: u64,

    /// Time-wheel consumers (one per producer thread).
    wheels: Vec<TimeWheelConsumer>,

    /// Kill queue consumer.
    kill_rx: KillQueueConsumer,
}

impl RfofsEngine {
    pub fn new(
        sample_rate: f32,
        pan_mode: PanMode,
        initial_capacity: usize,
        max_block_size: usize,
        wheels: Vec<TimeWheelConsumer>,
        kill_rx: KillQueueConsumer,
    ) -> Self {
        RfofsEngine {
            sample_rate,
            pan_mode,
            active: Slab::with_capacity(initial_capacity),
            id_map: HashMap::with_capacity(256),
            incoming: Vec::with_capacity(256),
            kill_requests: Vec::with_capacity(64),
            mono_buf: vec![0.0f32; max_block_size],
            gains_buf: vec![0.0f32; pan_mode.channel_count()],
            active_keys: Vec::with_capacity(initial_capacity),
            dead_keys: Vec::with_capacity(initial_capacity),
            sample_clock: 0,
            wheels,
            kill_rx,
        }
    }

    /// Process one block of audio.
    ///
    /// `outputs` is a flat slice of per-channel buffers, each of length
    /// `block_size`.  The caller must zero them before this call, or pass
    /// buffers that are already zeroed.
    ///
    /// Layout: `outputs[ch][sample]`.
    pub fn process_block(&mut self, outputs: &mut [&mut [f32]], block_size: usize) {
        let block_start = self.sample_clock;
        let block_end = block_start + block_size as u64;

        // ── 1. Drain queues ───────────────────────────────────────────────
        self.drain_wheels(block_start, block_size as u64);
        self.drain_kills();

        // ── 2. Spawn incoming FOFs ────────────────────────────────────────
        for params in self.incoming.drain(..) {
            let idx = self.active.insert(FofState::spawn(params, self.sample_rate));
            if params.id != 0 {
                self.id_map.insert(params.id, idx);
            }
        }

        // ── 3. Process kills ──────────────────────────────────────────────
        for req in self.kill_requests.drain(..) {
            if let Some(&idx) = self.id_map.get(&req.id) {
                self.active[idx].trigger_fade_out(req.fade_dur);
            }
        }

        // ── 4. Block-level DSP ───────────────────────────────────────────────
        if block_size > self.mono_buf.len() {
            self.mono_buf.resize(block_size, 0.0);
        }

        self.active_keys.clear();
        self.active_keys.extend(self.active.iter().map(|(k, _)| k));

        let sample_rate = self.sample_rate;
        let pan_mode    = self.pan_mode;
        let n_ch        = outputs.len();

        for &key in &self.active_keys {
            // 4a. Zero mono scratch for this FOF.
            let mono = &mut self.mono_buf[..block_size];
            for x in mono.iter_mut() { *x = 0.0; }

            // 4b. Fill mono block (handles sub-block start and mid-block Dead).
            self.active[key].fill_block(sample_rate, block_start, mono);

            // 4c. Compute static pan gains once per FOF.
            for x in self.gains_buf.iter_mut() { *x = 0.0; }
            let p = &self.active[key].params;
            pan_gains(p.azm, p.elev, p.distance, pan_mode, &mut self.gains_buf);

            // 4d. Scatter mono block into output channels.
            let mono = &self.mono_buf[..block_size];
            for (ch, &gain) in self.gains_buf.iter().enumerate().take(n_ch) {
                for (out_s, &m) in outputs[ch].iter_mut().zip(mono.iter()) {
                    *out_s += m * gain;
                }
            }
        }

        // ── 5. Remove dead FOFs ───────────────────────────────────────────
        self.dead_keys.clear();
        for (key, fof) in self.active.iter() {
            if fof.phase == FofPhase::Dead {
                self.dead_keys.push((key, fof.params.id));
            }
        }
        for (key, id) in self.dead_keys.drain(..) {
            self.active.remove(key);
            if id != 0 {
                self.id_map.remove(&id);
            }
        }

        // ── 6. Advance clock ──────────────────────────────────────────────
        self.sample_clock = block_end;
    }

    /// Current count of active FOFs.
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Current absolute sample clock (start of the next block).
    pub fn sample_clock(&self) -> u64 {
        self.sample_clock
    }

    /// Override the sample clock ahead of the next `process_block` call.
    ///
    /// Used by the JACK-mode caller to resync `sample_clock` to an external
    /// JACK time source (`jack_frame_time()` or the transport position)
    /// each cycle instead of relying purely on internal `block_size`
    /// accumulation — see `src/clock.rs`. Not used by `OfflineRenderer`,
    /// which has no JACK clock to sync to and relies on the free-running
    /// accumulation `process_block` already does.
    pub fn set_sample_clock(&mut self, sample: u64) {
        self.sample_clock = sample;
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn drain_wheels(&mut self, block_start: u64, block_size: u64) {
        for wheel in self.wheels.iter_mut() {
            wheel.drain_block_safe(block_start, block_size, &mut self.incoming);
        }
    }

    fn drain_kills(&mut self) {
        self.kill_rx.drain_all(&mut self.kill_requests);
    }
}
