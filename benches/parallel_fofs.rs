//! Benchmark for N FOFs that all start simultaneously (max instantaneous
//! polyphony), split into the two costs that matter separately in practice:
//!
//!  1. `enqueue`  — pushing N `FofParams` onto the time-wheel.
//!  2. `compute`  — running `process_block` until every FOF has died,
//!                  starting from a wheel that's already fully loaded.
//!
//! Both report throughput in FOFs/sec.

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use rfofs::{
    fof::FofParams,
    pan::PanMode,
    queue::{kill_queue, time_wheel},
    RfofsEngine,
};

const SAMPLE_RATE: f32 = 48000.0;
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
        distance: 0.0
    }
}

// const FOF_SIZES: &[usize] = &[100, 500, 1000, 2000];
const FOF_SIZES: &[usize] = &[100];

/// Time spent pushing N simultaneous-start FOFs onto the time-wheel.
fn bench_enqueue(c: &mut Criterion) {
    let mut group = c.benchmark_group("parallel_fofs/enqueue");

    for &n_fofs in FOF_SIZES {
        group.throughput(Throughput::Elements(n_fofs as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n_fofs), &n_fofs, |b, &n| {
            b.iter_batched(
                || {
                    let capacity = n.next_power_of_two();
                    // All FOFs share start_sample = 0, landing in a single
                    // slot -> slot_capacity must cover the whole batch.
                    let (tx, rx) = time_wheel(capacity, 4, BLOCK_SIZE as u64, capacity);
                    let params: Vec<FofParams> = (0..n).map(|_| standard_fof()).collect();
                    (tx, rx, params)
                },
                |(mut tx, rx, params)| {
                    for p in params {
                        tx.push(p).expect("queue full");
                    }
                    // Keep the consumer alive for the duration of the pushes;
                    // return it so it isn't dropped mid-timing.
                    black_box(rx)
                },
                BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

struct ComputeSetup {
    engine: RfofsEngine,
}

fn setup_compute(n_fofs: usize) -> ComputeSetup {
    let capacity = n_fofs.next_power_of_two();
    // All FOFs share start_sample = 0, landing in a single slot ->
    // slot_capacity must cover the whole batch.
    let (mut wheel_tx, wheel_rx) = time_wheel(capacity, 4, BLOCK_SIZE as u64, capacity);
    let (_kill_tx, kill_rx) = kill_queue(64);

    for _ in 0..n_fofs {
        wheel_tx.push(standard_fof()).expect("queue full");
    }
    drop(wheel_tx);

    let engine = RfofsEngine::new(
        SAMPLE_RATE,
        PanMode::Mono,
        n_fofs,
        BLOCK_SIZE,
        vec![wheel_rx],
        kill_rx,
    );

    ComputeSetup { engine }
}

/// Time spent computing all blocks from a fully-loaded wheel (all N FOFs
/// starting at sample 0) until every FOF has reached the `Dead` phase.
fn bench_compute(c: &mut Criterion) {
    let mut group = c.benchmark_group("parallel_fofs/compute");

    for &n_fofs in FOF_SIZES {
        group.throughput(Throughput::Elements(n_fofs as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n_fofs), &n_fofs, |b, &n| {
            b.iter_batched(
                || setup_compute(n),
                |mut s| {
                    let mut ch0 = vec![0.0f32; BLOCK_SIZE];
                    let mut ch1 = vec![0.0f32; BLOCK_SIZE];
                    let mut blocks = 0u64;
                    loop {
                        ch0.fill(0.0);
                        ch1.fill(0.0);
                        s.engine.process_block(&mut [&mut ch0, &mut ch1], BLOCK_SIZE);
                        blocks += 1;
                        if s.engine.active_count() == 0 {
                            break;
                        }
                    }
                    black_box((ch0, ch1, blocks))
                },
                BatchSize::LargeInput,
            )
        });
    }

    group.finish();
}

criterion_group!(benches, bench_enqueue, bench_compute);
criterion_main!(benches);
