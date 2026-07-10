//! Standalone profiling harness mirroring `parallel_fofs/compute`:
//! N FOFs all starting at sample 0, rendered until every one is Dead.
//! Reports throughput in FOFs/sec so it can be compared against the C
//! reference (37318 fofs/s) and profiled directly with perf/valgrind
//! without criterion's warmup/statistics machinery in the way.
//!
//! Usage: cargo run --release --example profile_compute [n_fofs] [n_reps]

use std::time::Instant;

use rfofs::{
    fof::FofParams,
    pan::PanMode,
    queue::{kill_queue, time_wheel},
    RfofsEngine,
};

const SAMPLE_RATE: f32 = 48000.0;
// Matches benches/parallel_fofs.rs (the C-reference comparison setup).
const BLOCK_SIZE: usize = 256;

fn standard_fof() -> FofParams {
    FofParams {
        id: 0,
        start_sample: 0,
        f: 1000.0,
        gliss: 0.0,
        phi: 0.0,
        amp: 1.0,
        alpha: 13.5,
        beta: 0.1,
        fade_level: 0.001,
        fade_dur: 0.005,
        azm: 0.0,
        elev: 0.0,
        distance: 0.0,
    }
}

fn make_engine(n_fofs: usize) -> RfofsEngine {
    let capacity = n_fofs.next_power_of_two();
    let (mut wheel_tx, wheel_rx) = time_wheel(capacity, 4, BLOCK_SIZE as u64, capacity);
    let (_kill_tx, kill_rx) = kill_queue(64);
    for _ in 0..n_fofs {
        wheel_tx.push(standard_fof()).expect("queue full");
    }
    drop(wheel_tx);
    RfofsEngine::new(SAMPLE_RATE, PanMode::Mono, n_fofs, BLOCK_SIZE, vec![wheel_rx], kill_rx)
}

fn run_once(n_fofs: usize) -> u64 {
    let mut engine = make_engine(n_fofs);
    let mut ch0 = vec![0.0f32; BLOCK_SIZE];
    let mut ch1 = vec![0.0f32; BLOCK_SIZE];
    let mut blocks = 0u64;
    loop {
        ch0.fill(0.0);
        ch1.fill(0.0);
        engine.process_block(&mut [&mut ch0, &mut ch1], BLOCK_SIZE);
        blocks += 1;
        if engine.active_count() == 0 {
            break;
        }
    }
    std::hint::black_box(ch0[0] + ch1[0]);
    blocks
}

fn main() {
    let mut args = std::env::args().skip(1);
    let n_fofs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1000);
    let n_reps: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);

    // Warm up.
    run_once(n_fofs);

    let start = Instant::now();
    let mut total_blocks = 0u64;
    for _ in 0..n_reps {
        total_blocks += run_once(n_fofs);
    }
    let elapsed = start.elapsed();

    let total_fofs = (n_fofs * n_reps) as f64;
    let throughput = total_fofs / elapsed.as_secs_f64();
    eprintln!(
        "n_fofs={n_fofs} reps={n_reps} blocks/rep={} total={:.3}s  throughput={:.0} fofs/s",
        total_blocks / n_reps as u64,
        elapsed.as_secs_f64(),
        throughput
    );
}
