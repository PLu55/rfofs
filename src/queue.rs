use std::sync::atomic::AtomicU64;
#[cfg(feature = "statistics")]
use std::sync::atomic::Ordering;

use rtrb::{Consumer, Producer, PushError, RingBuffer};
use crate::fof::{FofParams, FofKillRequest};

// ─────────────────────────────────────────────────────────────────────────────
// Time-wheel
// ─────────────────────────────────────────────────────────────────────────────

/// A single-level timing wheel: `N` slots, each spanning a fixed duration
/// `D` (samples) and holding up to `M` events. The wheel's horizon —
/// the furthest deadline it can currently accept — is `N * D`.
///
/// An event's deadline maps it into slot
/// `(current_slot + (deadline - now) / D) % N`. When the wheel's clock
/// advances past a slot's boundary, every event in that slot fires
/// together. Events deadlined beyond the horizon, or already in the past
/// relative to the wheel's clock, are rejected — there is no overflow list
/// or wraparound handling yet.
///
/// All `N` slots are preallocated to capacity `M` at construction so that
/// `schedule` never allocates on the hot (real-time) path.
/// Why [`Wheel::schedule`] rejected an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// `deadline` was already behind the wheel's clock.
    TooLate,
    /// `deadline` is at or beyond the wheel's current horizon — too far in
    /// the future for the wheel to accept yet.
    TooEarly,
    /// The event's target slot is already at capacity `M`.
    SlotFull,
}

struct Wheel<T> {
    slots: Vec<Vec<T>>,
    slot_duration: u64,
    slot_capacity: usize,
    /// Absolute slot counter since epoch (not reduced modulo `n_slots`).
    /// `slot_index % n_slots` gives the array index of the slot currently
    /// at the wheel's read head.
    slot_index: u64,
}

impl<T> Wheel<T> {
    fn new(n_slots: usize, slot_duration: u64, slot_capacity: usize) -> Self {
        assert!(n_slots > 0, "n_slots must be > 0");
        assert!(slot_duration > 0, "slot_duration must be > 0");
        Wheel {
            slots: (0..n_slots).map(|_| Vec::with_capacity(slot_capacity)).collect(),
            slot_duration,
            slot_capacity,
            slot_index: 0,
        }
    }

    fn n_slots(&self) -> usize {
        self.slots.len()
    }

    /// Total number of events currently sitting in the wheel's slots,
    /// scheduled but not yet fired. O(`n_slots`); only called once per
    /// audio block (from `drain_block_safe`), so this is negligible. Only
    /// used when the `statistics` feature is enabled (plus its own unit
    /// test), hence the `dead_code` allowance for builds without it.
    #[cfg_attr(not(feature = "statistics"), allow(dead_code))]
    fn len(&self) -> usize {
        self.slots.iter().map(Vec::len).sum()
    }

    /// Start-of-interval time of the slot currently at the read head.
    fn wheel_time(&self) -> u64 {
        self.slot_index * self.slot_duration
    }

    /// Furthest deadline (exclusive) the wheel can currently accept.
    fn horizon(&self) -> u64 {
        self.n_slots() as u64 * self.slot_duration
    }

    /// Schedule `event` to fire when the wheel's clock reaches `deadline`.
    ///
    /// On success, returns `slot_delta` — how many slots ahead of the
    /// wheel's current slot the event landed in (0 means the currently
    /// active slot). Rejects (returning the event back) if `deadline` is
    /// already behind the wheel's clock, at or beyond the horizon, or its
    /// target slot is already at capacity `M`.
    fn schedule(&mut self, event: T, deadline: u64) -> Result<u64, (T, RejectReason)> {
        let now = self.wheel_time();
        if deadline < now {
            return Err((event, RejectReason::TooLate)); // overdue — ignored
        }
        let offset = deadline - now;
        if offset >= self.horizon() {
            return Err((event, RejectReason::TooEarly)); // beyond horizon — ignored
        }
        let slot_delta = offset / self.slot_duration;
        let n_slots = self.n_slots() as u64;
        let idx = ((self.slot_index + slot_delta) % n_slots) as usize;
        let slot = &mut self.slots[idx];
        if slot.len() >= self.slot_capacity {
            return Err((event, RejectReason::SlotFull)); // slot full — ignored
        }
        slot.push(event);
        Ok(slot_delta)
    }

    /// Advance the wheel's clock to `now`, firing (draining, in order)
    /// every slot whose interval fully elapsed along the way.
    ///
    /// `now` is normally only ever a `block_size` or so past the wheel's
    /// current clock, so this is normally O(1). But callers may also resync
    /// the wheel's clock to an external, absolute time source (see
    /// `RfofsEngine::set_sample_clock`, used for `ClockMode::JackFrameTime`)
    /// whose origin predates the wheel's — e.g. `jack_frame_time()` counts
    /// from when the *JACK server* started, not from when this wheel was
    /// constructed, so the very first resync can jump `now` forward by
    /// billions of samples. Once the gap spans a full rotation
    /// (`n_slots` slots), every slot must still be drained once (any of
    /// them could hold a stale-but-undrained event), but no slot needs
    /// visiting more than once — anything further back than one rotation
    /// could only have fired already — so we jump `slot_index` straight to
    /// its target instead of stepping through the gap one slot at a time.
    fn advance(&mut self, now: u64, out: &mut Vec<T>) {
        let n_slots = self.n_slots() as u64;
        let target_index = now / self.slot_duration;
        if target_index <= self.slot_index {
            return;
        }
        let steps = (target_index - self.slot_index).min(n_slots);
        for i in 0..steps {
            let idx = ((self.slot_index + i) % n_slots) as usize;
            out.append(&mut self.slots[idx]);
        }
        self.slot_index = target_index;
    }
}

/// Width of [`QueueStats::slot_offset_histogram`], fixed at compile time so
/// the histogram can be embedded directly in the shared-memory control
/// block regardless of the wheel's actual `n_slots` (CLI-configurable, see
/// `main.rs`, and potentially far larger than this). Bucket `i` counts
/// events admitted exactly `i` slots ahead of the wheel's current slot, for
/// `i < SLOT_OFFSET_HISTOGRAM_BUCKETS - 1`; the last bucket is an overflow
/// counting everything `>= SLOT_OFFSET_HISTOGRAM_BUCKETS - 1` slots ahead.
pub const SLOT_OFFSET_HISTOGRAM_BUCKETS: usize = 64;

/// Lock-free counters tracking [`TimeWheelConsumer`] admission/scheduling
/// outcomes, plus current wheel occupancy. `#[repr(C)]` so it can be
/// embedded directly inside a shared-memory segment (see `shm.rs`) and read
/// by an external process without any locking or copying — the audio
/// thread updates these in place from [`TimeWheelConsumer::drain_block_safe`].
#[repr(C)]
pub struct QueueStats {
    /// Count of events rejected because their deadline had already passed
    /// (see [`RejectReason::TooLate`]).
    pub too_late: AtomicU64,
    /// Count of events rejected because their deadline was beyond the
    /// wheel's current horizon (see [`RejectReason::TooEarly`]). In
    /// practice this stays at 0 via [`TimeWheelConsumer::drain_block_safe`]:
    /// its `admit_before` pre-filter already excludes anything beyond
    /// horizon before `Wheel::schedule` is ever called, so this branch is
    /// only reachable if something calls `Wheel::schedule` directly.
    pub too_early: AtomicU64,
    /// Count of events rejected because their target slot was already at
    /// capacity `M` (see [`RejectReason::SlotFull`]).
    pub slot_full: AtomicU64,
    /// Number of events currently scheduled in the wheel, not yet fired.
    pub queue_size: AtomicU64,
    /// Histogram of how many slots ahead of the wheel's current slot each
    /// successfully admitted event landed in — see
    /// [`SLOT_OFFSET_HISTOGRAM_BUCKETS`]. Lets an external reader see
    /// whether onsets tend to arrive just-in-time (mass near bucket 0) or
    /// scheduled well ahead (mass in higher buckets, or the overflow one).
    pub slot_offset_histogram: [AtomicU64; SLOT_OFFSET_HISTOGRAM_BUCKETS],
}

impl Default for QueueStats {
    fn default() -> Self {
        QueueStats {
            too_late: AtomicU64::new(0),
            too_early: AtomicU64::new(0),
            slot_full: AtomicU64::new(0),
            queue_size: AtomicU64::new(0),
            slot_offset_histogram: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

/// Producer handle for a [`Wheel`]-backed FOF schedule.
///
/// Feeds a lock-free SPSC ring buffer (`rtrb`) — the cross-thread
/// transport — which the paired [`TimeWheelConsumer`] drains into its
/// internal `Wheel` as its clock advances. The producer must submit
/// `FofParams` in non-decreasing `start_sample` order: the consumer stops
/// draining the ring buffer as soon as it sees an entry beyond its current
/// admission window, assuming everything behind it is further out still.
pub struct TimeWheelProducer {
    tx: Producer<FofParams>,
}

/// Consumer handle for a [`Wheel`]-backed FOF schedule.
///
/// Owned by the audio thread. Each block, [`drain_block_safe`] admits
/// newly-ready entries from the ring buffer into the wheel and fires every
/// slot whose deadline has now elapsed.
///
/// [`drain_block_safe`]: TimeWheelConsumer::drain_block_safe
pub struct TimeWheelConsumer {
    rx: Consumer<FofParams>,
    wheel: Wheel<FofParams>,
    /// Optional stats sink, attached post-construction via [`attach_stats`]
    /// so [`time_wheel`]'s signature (and every existing caller/test) stays
    /// unchanged for callers that don't need stats tracking. Only present
    /// when the `statistics` feature is enabled — see [`attach_stats`].
    ///
    /// [`attach_stats`]: TimeWheelConsumer::attach_stats
    #[cfg(feature = "statistics")]
    stats: Option<&'static QueueStats>,
}

/// Create a matched producer/consumer pair.
///
/// - `ingress_capacity`: capacity of the SPSC ring buffer used to carry
///   raw `FofParams` from the producer thread to the consumer.
/// - `n_slots` / `slot_duration` / `slot_capacity`: the wheel's `N`, `D`
///   (samples), and `M` — see [`Wheel`]. Horizon is `n_slots * slot_duration`.
pub fn time_wheel(
    ingress_capacity: usize,
    n_slots: usize,
    slot_duration: u64,
    slot_capacity: usize,
) -> (TimeWheelProducer, TimeWheelConsumer) {
    let (tx, rx) = RingBuffer::new(ingress_capacity);
    (
        TimeWheelProducer { tx },
        TimeWheelConsumer {
            rx,
            wheel: Wheel::new(n_slots, slot_duration, slot_capacity),
            #[cfg(feature = "statistics")]
            stats: None,
        },
    )
}

impl TimeWheelProducer {
    /// Enqueue a FOF for scheduling.  Returns Err(params) if the buffer is full.
    pub fn push(&mut self, params: FofParams) -> Result<(), FofParams> {
        self.tx.push(params).map_err(|PushError::Full(v)| v)
    }

    /// Available write slots.
    pub fn slots_available(&self) -> usize {
        self.tx.slots()
    }
}

impl TimeWheelConsumer {
    /// Attach a stats sink — subsequent [`drain_block_safe`] calls will
    /// record admission/rejection counts and current queue occupancy into
    /// it. Typically points at a `QueueStats` embedded in a shared-memory
    /// segment (see `shm.rs`) so an external process can read it live.
    ///
    /// No-op when the `statistics` feature is disabled, so callers (e.g.
    /// `main.rs`) don't need to `cfg`-gate the call site.
    ///
    /// [`drain_block_safe`]: TimeWheelConsumer::drain_block_safe
    #[cfg(feature = "statistics")]
    pub fn attach_stats(&mut self, stats: &'static QueueStats) {
        self.stats = Some(stats);
    }

    #[cfg(not(feature = "statistics"))]
    pub fn attach_stats(&mut self, _stats: &'static QueueStats) {}

    /// Admit ready entries from the ring buffer into the wheel, then
    /// advance the wheel's clock to `block_start + block_size`, appending
    /// every FOF fired along the way to `out`.
    ///
    /// An entry is "ready" once its `start_sample` falls within the
    /// wheel's current admission window (i.e. it would not be rejected as
    /// beyond-horizon). Entries still further out are left in the ring
    /// buffer for a later call — they are deferred, not dropped. This
    /// relies on the producer's non-decreasing `start_sample` ordering:
    /// draining stops at the first not-yet-ready entry.
    pub fn drain_block_safe(&mut self, block_start: u64, block_size: u64, out: &mut Vec<FofParams>) {
        let admit_before = self.wheel.wheel_time() + self.wheel.horizon();
        while let Ok(chunk) = self.rx.read_chunk(1) {
            // SAFETY: chunk always has exactly 1 element.
            let params = *chunk.as_slices().0.first().unwrap();
            if params.start_sample < admit_before {
                chunk.commit_all(); // consume
                #[cfg_attr(not(feature = "statistics"), allow(unused_variables))]
                let result = self.wheel.schedule(params, params.start_sample);
                #[cfg(feature = "statistics")]
                if let Some(stats) = self.stats {
                    match result {
                        Ok(slot_delta) => {
                            let bucket =
                                (slot_delta as usize).min(SLOT_OFFSET_HISTOGRAM_BUCKETS - 1);
                            stats.slot_offset_histogram[bucket].fetch_add(1, Ordering::Relaxed);
                        }
                        Err((_, reason)) => {
                            let counter = match reason {
                                RejectReason::TooLate => &stats.too_late,
                                RejectReason::TooEarly => &stats.too_early,
                                RejectReason::SlotFull => &stats.slot_full,
                            };
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            } else {
                // Not yet within the admission window — leave in buffer.
                // Dropping chunk without committing returns the slot.
                break;
            }
        }
        let block_end = block_start + block_size;
        self.wheel.advance(block_end, out);
        #[cfg(feature = "statistics")]
        if let Some(stats) = self.stats {
            stats.queue_size.store(self.wheel.len() as u64, Ordering::Relaxed);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Kill queue
// ─────────────────────────────────────────────────────────────────────────────

pub struct KillQueueProducer {
    tx: Producer<FofKillRequest>,
}

pub struct KillQueueConsumer {
    rx: Consumer<FofKillRequest>,
}

pub fn kill_queue(capacity: usize) -> (KillQueueProducer, KillQueueConsumer) {
    let (tx, rx) = RingBuffer::new(capacity);
    (KillQueueProducer { tx }, KillQueueConsumer { rx })
}

impl KillQueueProducer {
    pub fn push(&mut self, req: FofKillRequest) -> Result<(), FofKillRequest> {
        self.tx.push(req).map_err(|PushError::Full(v)| v)
    }
}

impl KillQueueConsumer {
    /// Drain all pending kill requests into `out`.
    pub fn drain_all(&mut self, out: &mut Vec<FofKillRequest>) {
        while let Ok(req) = self.rx.pop() {
            out.push(req);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    fn params(start_sample: u64) -> FofParams {
        FofParams {
            id: 0,
            start_sample,
            f: 440.0,
            gliss: 0.0,
            phi: 0.0,
            amp: 1.0,
            alpha: 0.001,
            beta: 100.0,
            fade_level: 0.001,
            fade_dur: 50.0,
            azm: 0.0,
            elev: 0.0,
            distance: 1.0,
        }
    }

    fn kill(id: u64) -> FofKillRequest {
        FofKillRequest { id, fade_dur: 100.0 }
    }

    // ── Wheel<T> ─────────────────────────────────────────────────────────────

    #[test]
    fn wheel_event_not_fired_before_its_slot_border() {
        let mut w: Wheel<u64> = Wheel::new(4, 10, 8);
        w.schedule(1, 25).unwrap(); // slot 2: [20, 30)

        let mut out = Vec::new();
        w.advance(20, &mut out); // clock reaches 20, slot 2 not yet elapsed
        assert!(out.is_empty());
    }

    #[test]
    fn wheel_event_fires_once_its_slot_border_is_crossed() {
        let mut w: Wheel<u64> = Wheel::new(4, 10, 8);
        w.schedule(1, 25).unwrap(); // slot 2: [20, 30)

        let mut out = Vec::new();
        w.advance(30, &mut out); // clock reaches 30, slot 2 fully elapsed
        assert_eq!(out, vec![1]);
    }

    #[test]
    fn wheel_multiple_events_in_same_slot_fire_together_in_order() {
        let mut w: Wheel<u64> = Wheel::new(4, 10, 8);
        w.schedule(1, 21).unwrap();
        w.schedule(2, 25).unwrap();
        w.schedule(3, 29).unwrap();

        let mut out = Vec::new();
        w.advance(30, &mut out);
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn wheel_advance_spanning_multiple_borders_fires_oldest_slot_first() {
        let mut w: Wheel<u64> = Wheel::new(4, 10, 8);
        w.schedule(1, 5).unwrap();   // slot 0: [0, 10)
        w.schedule(2, 15).unwrap();  // slot 1: [10, 20)
        w.schedule(3, 25).unwrap();  // slot 2: [20, 30)

        let mut out = Vec::new();
        w.advance(30, &mut out);
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn wheel_rejects_event_beyond_horizon() {
        let mut w: Wheel<u64> = Wheel::new(4, 10, 8); // horizon = 40
        let (event, reason) = w.schedule(1, 40).unwrap_err();
        assert_eq!(event, 1);
        assert_eq!(reason, RejectReason::TooEarly);
    }

    #[test]
    fn wheel_accepts_event_at_last_valid_instant() {
        let mut w: Wheel<u64> = Wheel::new(4, 10, 8); // horizon = 40
        assert!(w.schedule(1, 39).is_ok());
    }

    #[test]
    fn wheel_rejects_overdue_event() {
        let mut w: Wheel<u64> = Wheel::new(4, 10, 8);
        let mut out = Vec::new();
        w.advance(15, &mut out); // wheel_time now 10
        let (event, reason) = w.schedule(1, 5).unwrap_err();
        assert_eq!(event, 1);
        assert_eq!(reason, RejectReason::TooLate);
    }

    #[test]
    fn wheel_rejects_push_into_full_slot_without_disturbing_existing() {
        let mut w: Wheel<u64> = Wheel::new(4, 10, 2);
        w.schedule(1, 1).unwrap();
        w.schedule(2, 2).unwrap();
        let (event, reason) = w.schedule(3, 3).unwrap_err();
        assert_eq!(event, 3);
        assert_eq!(reason, RejectReason::SlotFull);

        let mut out = Vec::new();
        w.advance(10, &mut out);
        assert_eq!(out, vec![1, 2]);
    }

    #[test]
    fn wheel_wraparound_reuses_slot_index_correctly() {
        let mut w: Wheel<u64> = Wheel::new(4, 10, 8);
        let mut out = Vec::new();
        // Advance clock past several full loops around the wheel (4 slots).
        w.advance(90, &mut out); // wheel_time now 90, slot_index 9
        out.clear();

        w.schedule(1, 95).unwrap(); // slot 9 % 4 == 1: [90, 100)
        w.advance(100, &mut out);
        assert_eq!(out, vec![1]);
    }

    #[test]
    fn wheel_len_tracks_scheduled_and_fired_events() {
        let mut w: Wheel<u64> = Wheel::new(4, 10, 8);
        assert_eq!(w.len(), 0);
        w.schedule(1, 5).unwrap();
        w.schedule(2, 15).unwrap();
        assert_eq!(w.len(), 2);

        let mut out = Vec::new();
        w.advance(10, &mut out); // fires slot 0 (event 1)
        assert_eq!(out, vec![1]);
        assert_eq!(w.len(), 1);
    }

    // ── TimeWheelProducer ────────────────────────────────────────────────────

    #[test]
    fn push_ok_when_space_available() {
        let (mut tx, _rx) = time_wheel(4, 4, 10, 8);
        assert!(tx.push(params(0)).is_ok());
    }

    #[test]
    fn push_err_when_full_returns_params() {
        let (mut tx, _rx) = time_wheel(2, 4, 10, 8);
        tx.push(params(1)).unwrap();
        tx.push(params(2)).unwrap();
        let err = tx.push(params(99)).unwrap_err();
        assert_eq!(err.start_sample, 99);
    }

    #[test]
    fn slots_available_decreases_on_push() {
        let (mut tx, _rx) = time_wheel(4, 4, 10, 8);
        let before = tx.slots_available();
        tx.push(params(0)).unwrap();
        assert_eq!(tx.slots_available(), before - 1);
    }

    // ── drain_block_safe ─────────────────────────────────────────────────────

    #[test]
    fn drain_block_safe_empties_items_within_window() {
        let (mut tx, mut rx) = time_wheel(8, 8, 16, 8); // horizon = 128
        tx.push(params(0)).unwrap();
        tx.push(params(64)).unwrap();
        tx.push(params(127)).unwrap();

        let mut out = Vec::new();
        rx.drain_block_safe(0, 128, &mut out);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn drain_block_safe_upper_bound_is_exclusive() {
        let (mut tx, mut rx) = time_wheel(8, 16, 10, 8); // D=10, horizon = 160
        tx.push(params(99)).unwrap();
        tx.push(params(100)).unwrap(); // == block_end, must NOT be included yet

        let mut out = Vec::new();
        rx.drain_block_safe(0, 100, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_sample, 99);
    }

    #[test]
    fn drain_block_safe_on_empty_produces_nothing() {
        let (_tx, mut rx) = time_wheel(4, 4, 10, 8);
        let mut out = Vec::new();
        rx.drain_block_safe(0, 128, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn drain_block_safe_consecutive_blocks() {
        let (mut tx, mut rx) = time_wheel(16, 16, 32, 8); // D=32, horizon=512
        for s in [0u64, 64, 128, 192] {
            tx.push(params(s)).unwrap();
        }

        let mut out = Vec::new();
        rx.drain_block_safe(0, 128, &mut out); // [0, 128)
        assert_eq!(
            out.iter().map(|p| p.start_sample).collect::<Vec<_>>(),
            [0, 64]
        );

        out.clear();
        rx.drain_block_safe(128, 128, &mut out); // [128, 256)
        assert_eq!(
            out.iter().map(|p| p.start_sample).collect::<Vec<_>>(),
            [128, 192]
        );
    }

    #[test]
    #[cfg(feature = "statistics")]
    fn drain_block_safe_tracks_queue_size_when_stats_attached() {
        let (mut tx, mut rx) = time_wheel(8, 8, 16, 8); // horizon = 128
        let stats: &'static QueueStats = Box::leak(Box::default());
        rx.attach_stats(stats);
        tx.push(params(0)).unwrap();
        tx.push(params(64)).unwrap();

        let mut out = Vec::new();
        rx.drain_block_safe(0, 16, &mut out); // only params(0) fires this block
        assert_eq!(out.len(), 1);
        assert_eq!(stats.queue_size.load(Ordering::Relaxed), 1); // params(64) still pending
    }

    #[test]
    #[cfg(feature = "statistics")]
    fn drain_block_safe_counts_slot_full_rejections_when_stats_attached() {
        let (mut tx, mut rx) = time_wheel(16, 4, 10, 2); // slot_capacity = 2
        let stats: &'static QueueStats = Box::leak(Box::default());
        rx.attach_stats(stats);
        // Three events land in the same slot ([0,10)); only 2 fit.
        tx.push(params(1)).unwrap();
        tx.push(params(2)).unwrap();
        tx.push(params(3)).unwrap();

        let mut out = Vec::new();
        rx.drain_block_safe(0, 10, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(stats.slot_full.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[cfg(feature = "statistics")]
    fn drain_block_safe_tracks_slot_offset_histogram_when_stats_attached() {
        let (mut tx, mut rx) = time_wheel(8, 8, 16, 8); // n_slots=8, D=16 -> horizon=128
        let stats: &'static QueueStats = Box::leak(Box::default());
        rx.attach_stats(stats);
        tx.push(params(0)).unwrap(); // lands in the current slot: slot_delta 0
        tx.push(params(32)).unwrap(); // 2 slots ahead: slot_delta 2

        let mut out = Vec::new();
        rx.drain_block_safe(0, 16, &mut out); // scheduling happens before the wheel advances
        assert_eq!(stats.slot_offset_histogram[0].load(Ordering::Relaxed), 1);
        assert_eq!(stats.slot_offset_histogram[2].load(Ordering::Relaxed), 1);
    }

    #[test]
    #[cfg(feature = "statistics")]
    fn drain_block_safe_clamps_far_slot_offsets_into_the_overflow_bucket() {
        let (mut tx, mut rx) = time_wheel(8, 200, 1, 8); // n_slots=200, D=1 -> horizon=200
        let stats: &'static QueueStats = Box::leak(Box::default());
        rx.attach_stats(stats);
        tx.push(params(150)).unwrap(); // slot_delta 150, far past the histogram's width

        let mut out = Vec::new();
        rx.drain_block_safe(0, 1, &mut out);
        assert_eq!(
            stats.slot_offset_histogram[SLOT_OFFSET_HISTOGRAM_BUCKETS - 1].load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn drain_block_safe_defers_entries_beyond_current_horizon_instead_of_dropping() {
        // Small horizon (n_slots=4, D=16 -> horizon=64) but an entry far
        // beyond it is queued up-front, as e.g. the process_block bench does
        // when it pre-loads all FOFs before any block runs.
        let (mut tx, mut rx) = time_wheel(8, 4, 16, 8);
        tx.push(params(10)).unwrap();
        tx.push(params(500)).unwrap(); // far beyond initial horizon of 64

        let mut out = Vec::new();
        rx.drain_block_safe(0, 64, &mut out);
        // Only the in-horizon entry fires; the far one is deferred, not lost.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_sample, 10);

        // Keep advancing the wheel's clock in block-sized steps until it
        // reaches the deferred entry's admission window.
        let mut block_start = 64u64;
        loop {
            out.clear();
            rx.drain_block_safe(block_start, 64, &mut out);
            if !out.is_empty() {
                break;
            }
            block_start += 64;
            assert!(block_start < 10_000, "deferred entry was lost, not just delayed");
        }
        assert_eq!(out[0].start_sample, 500);
    }

    // ── KillQueue ────────────────────────────────────────────────────────────

    #[test]
    fn kill_push_ok_when_space_available() {
        let (mut tx, _rx) = kill_queue(4);
        assert!(tx.push(kill(1)).is_ok());
    }

    #[test]
    fn kill_push_err_when_full_returns_request() {
        let (mut tx, _rx) = kill_queue(1);
        tx.push(kill(1)).unwrap();
        let err = tx.push(kill(99)).unwrap_err();
        assert_eq!(err.id, 99);
    }

    #[test]
    fn kill_drain_all_returns_all_in_order() {
        let (mut tx, mut rx) = kill_queue(8);
        tx.push(kill(1)).unwrap();
        tx.push(kill(2)).unwrap();
        tx.push(kill(3)).unwrap();

        let mut out = Vec::new();
        rx.drain_all(&mut out);
        assert_eq!(out.iter().map(|r| r.id).collect::<Vec<_>>(), [1, 2, 3]);
    }

    #[test]
    fn kill_drain_all_on_empty_produces_nothing() {
        let (_tx, mut rx) = kill_queue(4);
        let mut out = Vec::new();
        rx.drain_all(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn kill_drain_all_appends_to_existing_vec() {
        let (mut tx, mut rx) = kill_queue(4);
        tx.push(kill(10)).unwrap();

        let mut out = vec![kill(99)];
        rx.drain_all(&mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].id, 10);
    }
}
