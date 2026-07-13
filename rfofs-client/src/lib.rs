//! C-ABI client library for controlling a running `rfofs` process over its
//! shared-memory control plane (`rfofs::shm`). Meant to be loaded by an
//! external process — e.g. Racket via `ffi/unsafe` — to submit new FOF
//! onsets, request early fade-outs (kills), and read live queue stats.
//!
//! Every function here takes an opaque `*mut ClientHandle` obtained from
//! [`rfofs_connect`]. All functions are safe to call from any thread, but
//! (per the scope of `rfofs::shm`) only one client process is expected to
//! be attached to a given `rfofs` instance at a time.

use rfofs::fof::{FofKillRequest, FofParams};
use rfofs::shm::ClientShm;

/// Opaque handle returned by [`rfofs_connect`].
pub struct ClientHandle(ClientShm);

/// Snapshot of the queue stats living in the shared control block.
#[repr(C)]
pub struct RfofsStats {
    pub too_late: u64,
    pub too_early: u64,
    pub slot_full: u64,
    pub queue_size: u64,
}

/// Attempt to attach to an already-running `rfofs`'s control plane.
///
/// Returns null if no running `rfofs` was found, or if the segment found
/// doesn't match this build's wire format (magic/version mismatch).
#[unsafe(no_mangle)]
pub extern "C" fn rfofs_connect() -> *mut ClientHandle {
    match ClientShm::attach() {
        Ok(shm) => Box::into_raw(Box::new(ClientHandle(shm))),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Release a handle obtained from [`rfofs_connect`]. `handle` must not be
/// used again after this call. A null `handle` is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rfofs_disconnect(handle: *mut ClientHandle) {
    if handle.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(handle) });
}

/// Submit a new FOF onset. Field meanings match `rfofs::fof::FofParams`
/// exactly (see that type's doc comments): `id == 0` is fire-and-forget,
/// nonzero ids are individually killable via [`rfofs_kill`].
///
/// Returns 0 on success, -1 if `handle` is null, -2 if the shared request
/// ring is full (the caller is submitting faster than rfofs can drain it —
/// retry later).
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn rfofs_add_fof(
    handle: *mut ClientHandle,
    id: u64,
    start_sample: u64,
    f: f32,
    gliss: f32,
    phi: f32,
    amp: f32,
    alpha: f32,
    beta: f32,
    fade_level: f32,
    fade_dur: f32,
    azm: f32,
    elev: f32,
    distance: f32,
) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else { return -1 };
    let params = FofParams {
        id,
        start_sample,
        f,
        gliss,
        phi,
        amp,
        alpha,
        beta,
        fade_level,
        fade_dur,
        azm,
        elev,
        distance,
    };
    match handle.0.block().try_push_fof(params) {
        Ok(()) => 0,
        Err(_) => -2,
    }
}

/// Request an early fade-out on a tracked (nonzero-id) FOF. No-op on the
/// engine side if `id` doesn't match any currently active FOF.
///
/// Returns 0 on success, -1 if `handle` is null, -2 if the shared kill ring
/// is full.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rfofs_kill(handle: *mut ClientHandle, id: u64, fade_dur: f32) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else { return -1 };
    match handle.0.block().try_push_kill(FofKillRequest { id, fade_dur }) {
        Ok(()) => 0,
        Err(_) => -2,
    }
}

/// Read a live snapshot of the queue stats into `*out`.
///
/// Returns 0 on success, -1 if `handle` or `out` is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rfofs_get_stats(handle: *mut ClientHandle, out: *mut RfofsStats) -> i32 {
    let Some(handle) = (unsafe { handle.as_ref() }) else { return -1 };
    if out.is_null() {
        return -1;
    }
    let stats = &handle.0.block().stats;
    let snapshot = RfofsStats {
        too_late: stats.too_late.load(std::sync::atomic::Ordering::Relaxed),
        too_early: stats.too_early.load(std::sync::atomic::Ordering::Relaxed),
        slot_full: stats.slot_full.load(std::sync::atomic::Ordering::Relaxed),
        queue_size: stats.queue_size.load(std::sync::atomic::Ordering::Relaxed),
    };
    unsafe { out.write(snapshot) };
    0
}
