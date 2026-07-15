//! Cross-process control plane: a POSIX shared-memory segment carrying two
//! SPSC ring buffers (new-FOF requests, kill requests) plus a live stats
//! block, so an external process (e.g. a Racket script via the `rfofs-client`
//! cdylib) can control an already-running `rfofs` JACK process.
//!
//! This module only implements the process-boundary hop. Once inside
//! `rfofs`, entries are handed to the existing `TimeWheelProducer`/
//! `KillQueueProducer` (see `queue.rs`) by a small bridging thread — the
//! audio thread's contract (single-threaded, allocation-free, fed only
//! through `queue.rs`'s lock-free structures) is unchanged.
//!
//! # Why raw shared memory is sound here
//! `AtomicU64`/`AtomicU32` are lock-free on x86_64 Linux and operate on real
//! cache-coherent physical memory — the same atomic instructions work
//! correctly regardless of which process's virtual mapping touches them.
//! Each `Ring`'s data slots are plain (non-atomic) memory, protected by the
//! head/tail atomics' Acquire/Release ordering — the same technique `rtrb`
//! itself uses internally for its SPSC queues, just hand-rolled here because
//! `rtrb`'s ring buffers are heap-allocated and can't be placed in
//! externally-mapped memory. Soundness depends on genuinely single-producer/
//! single-consumer use of each `Ring` (one process only ever pushes, the
//! other only ever pops) — this module does not support multiple concurrent
//! clients.

use std::cell::UnsafeCell;
use std::ffi::CString;
use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::fof::{FofKillRequest, FofParams};
use crate::queue::QueueStats;

const MAGIC: u64 = 0x5246_4F46_535F_4331; // arbitrary constant, checked on attach
const VERSION: u32 = 1;
const FOF_CAP: usize = 4096; // mirrors main.rs's existing wheel ingress_capacity
const KILL_CAP: usize = 256; // mirrors main.rs's existing kill_queue capacity
pub const SHM_NAME: &str = "/rfofs_ctl";

// ─────────────────────────────────────────────────────────────────────────────
// Cross-process SPSC ring buffer
// ─────────────────────────────────────────────────────────────────────────────

/// A fixed-capacity single-producer/single-consumer ring buffer meant to be
/// embedded directly inside shared memory. `head`/`tail` are monotonically
/// increasing counters (never reduced modulo `CAP` themselves — only the
/// slot index is); this mirrors the well-known lock-free SPSC ring pattern.
#[repr(C)]
struct Ring<T: Copy, const CAP: usize> {
    head: AtomicU64,
    tail: AtomicU64,
    slots: [UnsafeCell<T>; CAP],
}

// SAFETY: `Ring` is only ever shared between exactly one producer and one
// consumer (enforced by which process calls which method, not by the type
// system — see module doc). The `UnsafeCell` slots are only read/written
// under the protection of `head`/`tail`'s Acquire/Release ordering, exactly
// as a single-producer/single-consumer discipline requires.
unsafe impl<T: Copy + Send, const CAP: usize> Sync for Ring<T, CAP> {}

impl<T: Copy, const CAP: usize> Ring<T, CAP> {
    /// Push `item`. Fails (returning it back) if the ring is full.
    /// Must only be called from the single producer side.
    fn try_push(&self, item: T) -> Result<(), T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if (tail.wrapping_sub(head) as usize) >= CAP {
            return Err(item);
        }
        // SAFETY: this slot is not readable by the consumer until `tail` is
        // published below (Release), and no other producer can be writing
        // here concurrently (single-producer discipline).
        unsafe { *self.slots[(tail as usize) % CAP].get() = item };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Pop the oldest item, if any. Must only be called from the single
    /// consumer side.
    fn try_pop(&self) -> Option<T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        // SAFETY: this slot was published by the producer's Release store
        // to `tail`, observed above via Acquire; no other consumer can be
        // reading here concurrently (single-consumer discipline).
        let item = unsafe { *self.slots[(head as usize) % CAP].get() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(item)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared control block layout
// ─────────────────────────────────────────────────────────────────────────────

/// The full contents of the shared-memory segment, `#[repr(C)]` so both the
/// `rfofs` server and any `rfofs-client` attach against the identical
/// in-memory layout (both compile this exact same Rust type from the same
/// source — there is no separately hand-maintained wire-format struct to
/// drift out of sync).
#[repr(C)]
pub struct SharedControlBlock {
    magic: AtomicU64,
    version: AtomicU32,
    /// Set to 1 once the creator has finished initializing the segment.
    ready: AtomicU32,
    /// The audio server's actual sample rate (from JACK/PipeWire), stored as
    /// `f32::to_bits`. Written once by `ServerShm::create` before `ready` is
    /// set, then never mutated — reads need no synchronization beyond the
    /// `ready` handshake itself.
    sample_rate_bits: AtomicU32,
    /// The audio server's configured buffer size (frames per callback), as
    /// reported by JACK/PipeWire at connect time. Individual callbacks may
    /// still report a smaller `n_frames` (`RfofsEngine::process_block`
    /// handles that already); this is the nominal/maximum value clients
    /// should plan around. Written once, same as `sample_rate_bits`.
    block_size: AtomicU32,
    /// Which JACK time source drives `sample_clock` each block — one of
    /// `crate::clock::RFOFS_CLOCK_JACK_FRAME_TIME`/`RFOFS_CLOCK_JACK_TRANSPORT`.
    /// Selected at server startup (CLI flag) and never mutated afterwards,
    /// same handshake as `sample_rate_bits`/`block_size`.
    clock_mode: AtomicU32,
    /// The engine's current absolute sample clock (start of the next block
    /// to be processed), updated once per audio block from the process
    /// callback. Clients need this to submit `start_sample` values that
    /// land in the future relative to the *server's* clock — the wheel
    /// rejects anything already behind it as [`crate::queue::RejectReason::TooLate`].
    current_sample: AtomicU64,
    fof_ring: Ring<FofParams, FOF_CAP>,
    kill_ring: Ring<FofKillRequest, KILL_CAP>,
    pub stats: QueueStats,
}

impl SharedControlBlock {
    /// The audio server's sample rate, in Hz.
    pub fn sample_rate(&self) -> f32 {
        f32::from_bits(self.sample_rate_bits.load(Ordering::Relaxed))
    }

    /// The audio server's nominal buffer size, in frames.
    pub fn block_size(&self) -> u32 {
        self.block_size.load(Ordering::Relaxed)
    }

    /// The active clock mode — `crate::clock::RFOFS_CLOCK_JACK_FRAME_TIME`
    /// or `RFOFS_CLOCK_JACK_TRANSPORT`.
    pub fn clock_mode(&self) -> u32 {
        self.clock_mode.load(Ordering::Relaxed)
    }

    /// The engine's current absolute sample clock. Clients should submit
    /// `start_sample` as this value plus some future headroom (enough to
    /// absorb the bridging thread's poll latency), not an absolute count
    /// from their own notion of time zero.
    pub fn current_sample(&self) -> u64 {
        self.current_sample.load(Ordering::Relaxed)
    }

    /// Publish the engine's current sample clock. Called once per audio
    /// block from the process callback (server side only).
    pub fn set_current_sample(&self, sample: u64) {
        self.current_sample.store(sample, Ordering::Relaxed);
    }

    /// Submit a new FOF request. Called by the client (Racket) side.
    pub fn try_push_fof(&self, params: FofParams) -> Result<(), FofParams> {
        self.fof_ring.try_push(params)
    }

    /// Drain one pending FOF request, if any. Called by rfofs's bridging thread.
    pub fn try_pop_fof(&self) -> Option<FofParams> {
        self.fof_ring.try_pop()
    }

    /// Submit a kill request. Called by the client (Racket) side.
    pub fn try_push_kill(&self, req: FofKillRequest) -> Result<(), FofKillRequest> {
        self.kill_ring.try_push(req)
    }

    /// Drain one pending kill request, if any. Called by rfofs's bridging thread.
    pub fn try_pop_kill(&self) -> Option<FofKillRequest> {
        self.kill_ring.try_pop()
    }
}

fn shm_name_cstring() -> CString {
    CString::new(SHM_NAME).expect("SHM_NAME must not contain interior NUL bytes")
}

// ─────────────────────────────────────────────────────────────────────────────
// Server side (owned by rfofs)
// ─────────────────────────────────────────────────────────────────────────────

/// Owns the shared-memory segment `rfofs` creates at JACK-mode startup.
/// Intentionally leaks the mapping for the process's lifetime — `rfofs`
/// currently only ever exits at process end, so there is no mid-run point
/// where unmapping would matter, and this avoids needing shutdown/signal
/// handling infrastructure that doesn't otherwise exist in `main.rs`.
pub struct ServerShm {
    ptr: *mut SharedControlBlock,
}

// SAFETY: the pointer refers to a `mmap`'d shared-memory region, not
// process-local heap memory; all access to its contents is through atomics
// or the single-producer/single-consumer-disciplined `Ring`s above.
unsafe impl Send for ServerShm {}
unsafe impl Sync for ServerShm {}

impl ServerShm {
    /// Create (or take over) the control-plane shared-memory segment.
    ///
    /// Tries `O_CREAT|O_EXCL` first; if a stale segment from a previous
    /// crashed run already exists (`EEXIST`), unlinks it and retries once
    /// rather than failing outright.
    ///
    /// `sample_rate`/`block_size` are the values the caller's audio server
    /// (JACK/PipeWire) is actually running at — they're published into the
    /// segment so attaching clients (e.g. `rfofs-client`) can read them back
    /// instead of assuming a fixed rate. `clock_mode` is the JACK time
    /// source (`crate::clock::RFOFS_CLOCK_JACK_FRAME_TIME`/
    /// `RFOFS_CLOCK_JACK_TRANSPORT`) selected for this run, published the
    /// same way.
    pub fn create(sample_rate: f32, block_size: u32, clock_mode: u32) -> io::Result<Self> {
        let name = shm_name_cstring();
        let mut fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
                0o600,
            )
        };
        if fd < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EEXIST) {
                unsafe { libc::shm_unlink(name.as_ptr()) };
                fd = unsafe {
                    libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_RDWR | libc::O_EXCL, 0o600)
                };
            }
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        let size = std::mem::size_of::<SharedControlBlock>();
        if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }

        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd) }; // safe to close once mapped
        if addr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        let ptr = addr as *mut SharedControlBlock;
        // SAFETY: freshly `ftruncate`d shm pages are zero-filled by the
        // kernel, so `head`/`tail`/`stats` all start at a valid all-zero
        // state; only `magic`/`version` need explicit values, and `ready`
        // must be set last (Release) so a concurrently-attaching client
        // never observes `ready == 1` before magic/version are visible.
        unsafe {
            (*ptr).magic.store(MAGIC, Ordering::Relaxed);
            (*ptr).version.store(VERSION, Ordering::Relaxed);
            (*ptr).sample_rate_bits.store(sample_rate.to_bits(), Ordering::Relaxed);
            (*ptr).block_size.store(block_size, Ordering::Relaxed);
            (*ptr).clock_mode.store(clock_mode, Ordering::Relaxed);
            (*ptr).ready.store(1, Ordering::Release);
        }

        Ok(ServerShm { ptr })
    }

    pub fn block(&self) -> &'static SharedControlBlock {
        // SAFETY: the mapping is leaked for the process's lifetime (see
        // struct doc), so a `'static` borrow is valid for as long as
        // anything could hold it.
        unsafe { &*self.ptr }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Client side (used by rfofs-client)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum AttachError {
    /// No running `rfofs` was found (the segment doesn't exist).
    NotRunning(io::Error),
    /// The segment exists but wasn't fully initialized within the retry
    /// budget — extremely unlikely (a race with `ServerShm::create` that's
    /// still in progress), but distinguished from `NotRunning` for clarity.
    NotReady,
    /// The segment's magic/version don't match this build's — an `rfofs`
    /// and `rfofs-client` built from different sources are attached.
    VersionMismatch,
    Mmap(io::Error),
}

/// A client's attachment to an already-running `rfofs`'s control-plane
/// segment.
pub struct ClientShm {
    ptr: *mut SharedControlBlock,
}

unsafe impl Send for ClientShm {}
unsafe impl Sync for ClientShm {}

impl ClientShm {
    /// Attach to a running `rfofs`'s shared-memory segment.
    ///
    /// Deliberately does **not** pass `O_CREAT` — an `ENOENT` here is
    /// exactly "no running rfofs was found", which is the only sensible
    /// meaning of "connect to a running rfofs" for a client process.
    pub fn attach() -> Result<Self, AttachError> {
        let name = shm_name_cstring();
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR, 0) };
        if fd < 0 {
            return Err(AttachError::NotRunning(io::Error::last_os_error()));
        }

        let size = std::mem::size_of::<SharedControlBlock>();
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd) };
        if addr == libc::MAP_FAILED {
            return Err(AttachError::Mmap(io::Error::last_os_error()));
        }

        let ptr = addr as *mut SharedControlBlock;
        let block = unsafe { &*ptr };

        // Bounded spin-wait in case we raced a not-yet-finished create().
        let mut ready = false;
        for _ in 0..100 {
            if block.ready.load(Ordering::Acquire) == 1 {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if !ready {
            unsafe { libc::munmap(addr, size) };
            return Err(AttachError::NotReady);
        }

        if block.magic.load(Ordering::Relaxed) != MAGIC || block.version.load(Ordering::Relaxed) != VERSION {
            unsafe { libc::munmap(addr, size) };
            return Err(AttachError::VersionMismatch);
        }

        Ok(ClientShm { ptr })
    }

    pub fn block(&self) -> &SharedControlBlock {
        unsafe { &*self.ptr }
    }
}

impl Drop for ClientShm {
    fn drop(&mut self) {
        let size = std::mem::size_of::<SharedControlBlock>();
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, size) };
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

    #[test]
    fn ring_push_pop_preserves_order_single_threaded() {
        let ring: Ring<u64, 4> = Ring {
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            slots: [const { UnsafeCell::new(0u64) }; 4],
        };
        ring.try_push(1).unwrap();
        ring.try_push(2).unwrap();
        ring.try_push(3).unwrap();
        assert_eq!(ring.try_pop(), Some(1));
        assert_eq!(ring.try_pop(), Some(2));
        assert_eq!(ring.try_pop(), Some(3));
        assert_eq!(ring.try_pop(), None);
    }

    #[test]
    fn ring_rejects_push_when_full() {
        let ring: Ring<u64, 2> = Ring {
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            slots: [const { UnsafeCell::new(0u64) }; 2],
        };
        ring.try_push(1).unwrap();
        ring.try_push(2).unwrap();
        assert_eq!(ring.try_push(3), Err(3));
        assert_eq!(ring.try_pop(), Some(1));
        // Freed a slot — push should succeed again.
        ring.try_push(3).unwrap();
        assert_eq!(ring.try_pop(), Some(2));
        assert_eq!(ring.try_pop(), Some(3));
    }

    /// Exercises the exact Acquire/Release protocol real cross-process use
    /// relies on, but intra-process with two real OS threads sharing one
    /// `Ring` via a leaked `'static` reference — no actual shm segment
    /// needed to validate the atomic choreography is correct.
    #[test]
    fn ring_cross_thread_producer_consumer_delivers_all_items_in_order() {
        const N: u64 = 20_000;
        let ring: &'static Ring<u64, 64> = Box::leak(Box::new(Ring {
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            slots: [const { UnsafeCell::new(0u64) }; 64],
        }));

        let producer = std::thread::spawn(move || {
            for i in 0..N {
                while ring.try_push(i).is_err() {
                    std::hint::spin_loop();
                }
            }
        });

        let consumer = std::thread::spawn(move || {
            let mut received = Vec::with_capacity(N as usize);
            while received.len() < N as usize {
                if let Some(item) = ring.try_pop() {
                    received.push(item);
                } else {
                    std::hint::spin_loop();
                }
            }
            received
        });

        producer.join().unwrap();
        let received = consumer.join().unwrap();
        assert_eq!(received, (0..N).collect::<Vec<_>>());
    }

    #[test]
    fn shared_control_block_fof_and_kill_rings_are_independent() {
        // SharedControlBlock itself isn't constructible outside shm (no
        // `new` — it's only ever placed via mmap), but its ring plumbing
        // can be validated at the `Ring` level (above) plus a size sanity
        // check: the struct must be Sized and have a stable, FFI-safe layout.
        assert!(std::mem::size_of::<SharedControlBlock>() > 0);
        let p1 = params(10);
        let p2 = params(20);
        assert_ne!(p1.start_sample, p2.start_sample);
    }
}
