use std::collections::HashMap;
use slab::Slab;

use crate::fof::{FofKillRequest, FofParams, FofPhase, FofState};
use crate::pan::{pan_sample, PanMode};
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

        // ── 4. Per-sample DSP ─────────────────────────────────────────────
        // Collect slab keys upfront to allow mutable borrow inside the loop.
        // Slab iteration is O(capacity) not O(len) — acceptable for dense pools.
        let keys: Vec<usize> = self.active.iter().map(|(k, _)| k).collect();

        // Reusable per-sample pan output scratch (stack allocation, small).
        let n_ch = self.pan_mode.channel_count();
        let mut pan_buf = vec![0.0f32; n_ch];

        for sample_idx in 0..block_size {
            let abs_sample = block_start + sample_idx as u64;

            for &key in &keys {
                let fof = &mut self.active[key];

                // Sub-block accurate start: skip if not yet started.
                if fof.params.start_sample > abs_sample {
                    continue;
                }

                let mono = fof.next_sample(self.sample_rate);

                // Clear pan scratch buffer.
                for x in pan_buf.iter_mut() { *x = 0.0; }

                pan_sample(
                    mono,
                    fof.params.azm,
                    fof.params.elev,
                    fof.params.distance,
                    self.pan_mode,
                    &mut pan_buf,
                );

                // Accumulate into output channels.
                for (ch, &gain) in pan_buf.iter().enumerate() {
                    if ch < outputs.len() {
                        outputs[ch][sample_idx] += gain;
                    }
                }
            }
        }

        // ── 5. Remove dead FOFs ───────────────────────────────────────────
        let mut dead_keys = Vec::new(); // small, avoids borrow conflict
        for (key, fof) in self.active.iter() {
            if fof.phase == FofPhase::Dead {
                dead_keys.push((key, fof.params.id));
            }
        }
        for (key, id) in dead_keys {
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
