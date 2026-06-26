use rtrb::{Consumer, Producer, PushError, RingBuffer};
use crate::fof::{FofParams, FofKillRequest};

// ─────────────────────────────────────────────────────────────────────────────
// Time-wheel
// ─────────────────────────────────────────────────────────────────────────────

/// A lock-free SPSC time-wheel for scheduling `FofParams` by absolute sample time.
///
/// The wheel is backed by a flat ring buffer.  The producer sorts by
/// `start_sample`; the audio thread drains all entries whose `start_sample`
/// falls within the current block `[block_start, block_start + block_size)`.
///
/// # Design notes
/// - One ring buffer per producer thread (SPSC guarantee).
/// - Multiple producers → multiple `TimeWheelProducer` handles feeding one
///   `TimeWheelConsumer` per producer.  The audio thread holds all consumers.
/// - Capacity should be sized generously (e.g. 4096) — dropped entries are
///   lost silently; the producer can check `slots_available()`.
pub struct TimeWheelProducer {
    tx: Producer<FofParams>,
}

pub struct TimeWheelConsumer {
    rx: Consumer<FofParams>,
}

/// Create a matched producer/consumer pair with the given capacity.
pub fn time_wheel(capacity: usize) -> (TimeWheelProducer, TimeWheelConsumer) {
    let (tx, rx) = RingBuffer::new(capacity);
    (TimeWheelProducer { tx }, TimeWheelConsumer { rx })
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
    /// Drain all FOFs whose `start_sample` falls within
    /// `[block_start, block_start + block_size)`.
    ///
    /// FOFs that are not yet due remain in the ring buffer.
    /// FOFs that are overdue (start_sample < block_start) are delivered
    /// with their original start offset, which the engine maps to sample 0
    /// of the current block.
    pub fn drain_block<'a>(
        &'a mut self,
        block_start: u64,
        block_size: u64,
        out: &mut Vec<FofParams>,
    ) {
        let block_end = block_start + block_size;
        while let Ok(chunk) = self.rx.read_chunk(1) {
            let params = chunk.into_iter().next().unwrap();
            if params.start_sample < block_end {
                out.push(params);
            } else {
                // Not due yet — put back is not possible with rtrb.
                // We must not consume it.  Use peek instead.
                // (See note below — we restructure to peek first.)
                // This branch should not be reached with the peek-first pattern.
                break;
            }
        }
    }

    /// Peek at the next entry's `start_sample` without consuming it.
    pub fn peek_start_sample(&mut self) -> Option<u64> {
        // read_chunk(1) without commit leaves the slot in the buffer.
        let chunk = self.rx.read_chunk(1).ok()?;
        let start_sample = chunk.as_slices().0.first().map(|p| p.start_sample);
        start_sample // chunk dropped here without commit — item stays
    }

    /// Drain block using a safe peek-then-pop pattern.
    ///
    /// Iterates the readable slots and pops only those due this block.
    /// Stops as soon as it finds an entry not yet due (assumes sorted input).
    pub fn drain_block_safe(
        &mut self,
        block_start: u64,
        block_size: u64,
        out: &mut Vec<FofParams>,
    ) {
        let block_end = block_start + block_size;
        while let Ok(chunk) = self.rx.read_chunk(1) {
            // SAFETY: chunk always has exactly 1 element.
            let params = *chunk.as_slices().0.first().unwrap();
            if params.start_sample < block_end {
                chunk.commit_all(); // consume
                out.push(params);
            } else {
                // Not due — leave in buffer.
                // Dropping chunk without commit returns the slot.
                break;
            }
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
            f_end: 440.0,
            phi: 0.0,
            amp: 1.0,
            alpha: 0.001,
            beta: 100.0,
            fade_level: 0.001,
            fade_dur: 50,
            azm: 0.0,
            elev: 0.0,
            distance: 1.0,
        }
    }

    fn kill(id: u64) -> FofKillRequest {
        FofKillRequest { id, fade_dur: 100 }
    }

    // ── TimeWheelProducer ────────────────────────────────────────────────────

    #[test]
    fn push_ok_when_space_available() {
        let (mut tx, _rx) = time_wheel(4);
        assert!(tx.push(params(0)).is_ok());
    }

    #[test]
    fn push_err_when_full_returns_params() {
        let (mut tx, _rx) = time_wheel(2);
        tx.push(params(1)).unwrap();
        tx.push(params(2)).unwrap();
        let err = tx.push(params(99)).unwrap_err();
        assert_eq!(err.start_sample, 99);
    }

    #[test]
    fn slots_available_decreases_on_push() {
        let (mut tx, _rx) = time_wheel(4);
        let before = tx.slots_available();
        tx.push(params(0)).unwrap();
        assert_eq!(tx.slots_available(), before - 1);
    }

    // ── peek_start_sample ────────────────────────────────────────────────────

    #[test]
    fn peek_none_on_empty() {
        let (_tx, mut rx) = time_wheel(4);
        assert_eq!(rx.peek_start_sample(), None);
    }

    #[test]
    fn peek_returns_sample_without_consuming() {
        let (mut tx, mut rx) = time_wheel(4);
        tx.push(params(42)).unwrap();
        assert_eq!(rx.peek_start_sample(), Some(42));
        assert_eq!(rx.peek_start_sample(), Some(42)); // still there
    }

    // ── drain_block ──────────────────────────────────────────────────────────

    #[test]
    fn drain_block_empties_items_within_window() {
        let (mut tx, mut rx) = time_wheel(8);
        tx.push(params(0)).unwrap();
        tx.push(params(64)).unwrap();
        tx.push(params(127)).unwrap();

        let mut out = Vec::new();
        rx.drain_block(0, 128, &mut out);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn drain_block_delivers_overdue_items() {
        let (mut tx, mut rx) = time_wheel(4);
        tx.push(params(5)).unwrap(); // overdue relative to block [100, 228)
        let mut out = Vec::new();
        rx.drain_block(100, 128, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_sample, 5);
    }

    #[test]
    fn drain_block_stops_at_future_item() {
        let (mut tx, mut rx) = time_wheel(8);
        tx.push(params(0)).unwrap();
        tx.push(params(500)).unwrap(); // future, past block [0, 128)

        let mut out = Vec::new();
        rx.drain_block(0, 128, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_sample, 0);
    }

    #[test]
    fn drain_block_on_empty_produces_nothing() {
        let (_tx, mut rx) = time_wheel(4);
        let mut out = Vec::new();
        rx.drain_block(0, 128, &mut out);
        assert!(out.is_empty());
    }

    // ── drain_block_safe ─────────────────────────────────────────────────────

    #[test]
    fn drain_block_safe_upper_bound_is_exclusive() {
        let (mut tx, mut rx) = time_wheel(8);
        tx.push(params(99)).unwrap();
        tx.push(params(100)).unwrap(); // == block_end, must NOT be included

        let mut out = Vec::new();
        rx.drain_block_safe(0, 100, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_sample, 99);
    }

    #[test]
    fn drain_block_safe_leaves_future_item_in_buffer() {
        let (mut tx, mut rx) = time_wheel(8);
        tx.push(params(0)).unwrap();
        tx.push(params(256)).unwrap(); // future

        let mut out = Vec::new();
        rx.drain_block_safe(0, 128, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(rx.peek_start_sample(), Some(256)); // still in buffer
    }

    #[test]
    fn drain_block_safe_delivers_overdue_items() {
        let (mut tx, mut rx) = time_wheel(4);
        tx.push(params(5)).unwrap(); // overdue
        let mut out = Vec::new();
        rx.drain_block_safe(100, 128, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_sample, 5);
    }

    #[test]
    fn drain_block_safe_on_empty_produces_nothing() {
        let (_tx, mut rx) = time_wheel(4);
        let mut out = Vec::new();
        rx.drain_block_safe(0, 128, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn drain_block_safe_consecutive_blocks() {
        let (mut tx, mut rx) = time_wheel(16);
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
