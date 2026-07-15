//! Manual end-to-end smoke test for the shared-memory control plane.
//!
//! Run `cargo run --release` in the rfofs crate first (starts the JACK
//! process and creates the control-plane shm segment), then in a second
//! terminal run:
//!
//!   cargo run --release -p rfofs-client --example shm_client_smoke
//!
//! This connects, submits a FOF (audible if rfofs's JACK ports are routed
//! to an output), submits a kill for a tracked id, and prints live stats.
//! Run it *without* rfofs running first to confirm rfofs_connect() fails
//! gracefully (returns null) instead of crashing.

use rfofs_client::{
    rfofs_add_fof, rfofs_block_size, rfofs_clock_mode, rfofs_connect, rfofs_current_sample,
    rfofs_disconnect, rfofs_get_stats, rfofs_kill, rfofs_sample_rate, RfofsStats,
    RFOFS_CLOCK_JACK_FRAME_TIME, RFOFS_CLOCK_JACK_TRANSPORT,
};

fn main() {
    let handle = rfofs_connect();
    if handle.is_null() {
        eprintln!("rfofs_connect failed — is rfofs running (cargo run --release)?");
        std::process::exit(1);
    }
    println!("connected to running rfofs");
    let sample_rate = unsafe { rfofs_sample_rate(handle) };
    let block_size = unsafe { rfofs_block_size(handle) };
    let clock_mode = unsafe { rfofs_clock_mode(handle) };
    let clock_mode_name = match clock_mode {
        RFOFS_CLOCK_JACK_FRAME_TIME => "jack-frame-time",
        RFOFS_CLOCK_JACK_TRANSPORT => "jack-transport",
        _ => "unknown",
    };
    println!(
        "server sample_rate={sample_rate} block_size={block_size} clock_mode={clock_mode} ({clock_mode_name})"
    );

    // start_sample is an absolute sample count on the *server's* clock, not
    // relative to when this client connects — the server has typically been
    // running for a while already, so submitting start_sample=0 lands the
    // FOF in the wheel long after that deadline has passed (rejected as
    // too_late). Anchor off the server's live clock instead, with enough
    // headroom (~0.2s) to absorb the bridging thread's poll latency.
    let now = unsafe { rfofs_current_sample(handle) };
    let headroom = (sample_rate * 0.2) as u64;
    let start_sample = now + headroom;

    // Fire-and-forget FOF, audible ~0.2s from now if routed to an output.
    let rc = unsafe {
        rfofs_add_fof(
            handle, 0, /* id */
            start_sample,
            440.0, 0.0, 0.0, 0.5, 10.0, 0.01, 0.001, 0.01, 0.0, 0.0, 1.0,
        )
    };
    println!("rfofs_add_fof (fire-and-forget) -> {rc}");

    // Tracked FOF (id = 42) immediately followed by a kill request.
    let rc = unsafe {
        rfofs_add_fof(
            handle, 42, start_sample, 220.0, 0.0, 0.0, 0.5, 2.0, 0.01, 0.001, 0.01, 0.0, 0.0, 1.0,
        )
    };
    println!("rfofs_add_fof (id=42) -> {rc}");
    let rc = unsafe { rfofs_kill(handle, 42, 0.05) };
    println!("rfofs_kill (id=42) -> {rc}");

    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut stats = RfofsStats { too_late: 0, too_early: 0, slot_full: 0, queue_size: 0 };
    let rc = unsafe { rfofs_get_stats(handle, &mut stats) };
    println!(
        "rfofs_get_stats -> {rc}: too_late={} too_early={} slot_full={} queue_size={}",
        stats.too_late, stats.too_early, stats.slot_full, stats.queue_size
    );

    unsafe { rfofs_disconnect(handle) };
    println!("disconnected");
}
