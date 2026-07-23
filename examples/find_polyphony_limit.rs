//! Finds the sustained real-time polyphony ceiling of
//! `RfofsEngine::process_block` on this machine: the largest N for which
//! processing N simultaneously-active FOFs still comfortably fits inside
//! one JACK block period, so a live server wouldn't start dropping onsets
//! (`RejectReason::TooLate`, see `src/queue.rs`) or under-running.
//!
//! Unlike `benches/parallel_fofs.rs`/`profile_compute.rs`, which average
//! cost over a FOF's whole decay-to-death lifetime (so N drops as voices
//! die, pulling the average below the true peak load), this keeps all N
//! FOFs alive for the whole measurement window (very slow decay,
//! `alpha` tiny) and times each block individually — the metric that
//! actually determines whether a live server glitches under sustained
//! polyphony.
//!
//! Usage: cargo run --release --example find_polyphony_limit [n1 n2 ...]
//! (defaults to a preset sweep if no sizes are given)

use std::time::Instant;

use rfofs::{
    fof::FofParams,
    pan::PanMode,
    queue::{kill_queue, time_wheel},
    RfofsEngine,
};

// Matches the block_size/sample_rate actually observed from the real JACK
// server (PipeWire) in this environment — see tests/online_stress.rs.
const SAMPLE_RATE: f32 = 48000.0;
const BLOCK_SIZE: usize = 256;
const BLOCK_PERIOD_SECS: f64 = BLOCK_SIZE as f64 / SAMPLE_RATE as f64;

const N_WARMUP_BLOCKS: usize = 20;
const N_MEASURED_BLOCKS: usize = 300;

/// A FOF that stays in `Attack`/`Decay` for far longer than the measurement
/// window (natural duration ~= -ln(fade_level)/alpha ~= 690s), so the
/// active count stays exactly N throughout — no voices die mid-measurement
/// to quietly lower the load.
fn sustained_fof() -> FofParams {
    FofParams {
        id: 0,
        start_sample: 0,
        f: 1000.0,
        gliss: 0.0,
        phi: 0.0,
        amp: 1.0,
        alpha: 0.01,
        beta: 0.01,
        fade_level: 0.001,
        fade_dur: 0.01,
        azm: 0.0,
        elev: 0.0,
        distance: 0.0,
    }
}

fn make_engine(n: usize) -> RfofsEngine {
    let capacity = n.next_power_of_two().max(1);
    // All FOFs share start_sample = 0, landing in a single slot -> slot
    // capacity must cover the whole batch (same trick as
    // benches/parallel_fofs.rs's setup_compute).
    let (mut wheel_tx, wheel_rx) = time_wheel(capacity, 4, BLOCK_SIZE as u64, capacity);
    let (_kill_tx, kill_rx) = kill_queue(64);
    for _ in 0..n {
        wheel_tx.push(sustained_fof()).expect("queue full");
    }
    drop(wheel_tx);
    RfofsEngine::new(SAMPLE_RATE, PanMode::Stereo, n, BLOCK_SIZE, vec![wheel_rx], kill_rx)
}

/// Per-block wall-clock durations (seconds) for `n_warmup_blocks +
/// n_measured_blocks` blocks of N constantly-active FOFs, with the warmup
/// prefix discarded.
fn measure(n: usize) -> Vec<f64> {
    let mut engine = make_engine(n);
    let mut ch0 = vec![0.0f32; BLOCK_SIZE];
    let mut ch1 = vec![0.0f32; BLOCK_SIZE];

    let mut durations = Vec::with_capacity(N_MEASURED_BLOCKS);
    for i in 0..(N_WARMUP_BLOCKS + N_MEASURED_BLOCKS) {
        ch0.fill(0.0);
        ch1.fill(0.0);
        let t0 = Instant::now();
        engine.process_block(&mut [&mut ch0, &mut ch1], BLOCK_SIZE);
        let elapsed = t0.elapsed().as_secs_f64();
        if i >= N_WARMUP_BLOCKS {
            durations.push(elapsed);
        }
    }
    std::hint::black_box((ch0, ch1));
    assert_eq!(
        engine.active_count(),
        n,
        "some FOFs reached Dead mid-measurement for n={n} -- alpha too high for the window, \
         results below are not comparing equal load"
    );
    durations
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx]
}

fn main() {
    let args: Vec<usize> = std::env::args().skip(1).filter_map(|s| s.parse().ok()).collect();
    let sizes: Vec<usize> = if args.is_empty() {
        vec![
            100, 500, 1000, 1500, 2000, 2500, 3000, 3500, 4000, 4500, 5000, 6000, 7000, 8000,
            10000, 12000, 15000, 20000,
        ]
    } else {
        args
    };

    println!(
        "block period budget: {:.4} ms ({BLOCK_SIZE} samples @ {SAMPLE_RATE} Hz), stereo, release build",
        BLOCK_PERIOD_SECS * 1000.0
    );
    println!("{:>7} {:>10} {:>10} {:>10} {:>9}", "N", "median_ms", "p95_ms", "max_ms", "budget%");

    let mut first_over_median: Option<usize> = None;
    let mut first_over_p95: Option<usize> = None;

    for &n in &sizes {
        let mut durations = measure(n);
        durations.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = percentile(&durations, 0.5);
        let p95 = percentile(&durations, 0.95);
        let max = *durations.last().unwrap();

        if median > BLOCK_PERIOD_SECS && first_over_median.is_none() {
            first_over_median = Some(n);
        }
        if p95 > BLOCK_PERIOD_SECS && first_over_p95.is_none() {
            first_over_p95 = Some(n);
        }

        println!(
            "{:>7} {:>10.4} {:>10.4} {:>10.4} {:>8.1}%",
            n,
            median * 1000.0,
            p95 * 1000.0,
            max * 1000.0,
            (median / BLOCK_PERIOD_SECS) * 100.0
        );
    }

    println!();
    match first_over_median {
        Some(n) => println!("median block time first exceeds the real-time budget at N={n}"),
        None => println!("median block time stayed under budget for every N tried"),
    }
    match first_over_p95 {
        Some(n) => println!("p95 block time first exceeds the real-time budget at N={n}"),
        None => println!("p95 block time stayed under budget for every N tried"),
    }
}
