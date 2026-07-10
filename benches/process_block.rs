use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use rfofs::{
    fof::FofParams,
    pan::PanMode,
    queue::{kill_queue, time_wheel},
    RfofsEngine,
};

const SAMPLE_RATE: f32 = 48000.0;
const BLOCK_SIZE: usize = 512;
// 10 ms inter-arrival → ~100 FOFs/sec; FOF duration ≈ 345 ms → ~34 simultaneous at steady state
const MEAN_INTER_ARRIVAL: f64 = 480.0;
// -ln(0.001) / 20.0 * 48000
const FOF_DURATION_SAMPLES: u64 = 16_573;

fn standard_fof(start_sample: u64) -> FofParams {
    FofParams {
        id: 0,
        start_sample,
        f: 440.0,
        gliss: 0.0,
        phi: 0.0,
        amp: 1.0,
        alpha: 20.0,
        beta: 0.01,
        fade_level: 0.001,
        fade_dur: 0.01,
        azm: 0.0,
        elev: 0.0,
        distance: 1.0,
    }
}

fn xorshift64(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn exp_sample(s: &mut u64) -> f64 {
    // Uniform in (0, 1] via 53-bit mantissa, then inverse-CDF for Exp(1)
    let bits = xorshift64(s) >> 11;
    let u = (bits as f64 + 1.0) / ((1u64 << 53) as f64 + 1.0);
    -u.ln()
}

struct Setup {
    engine: RfofsEngine,
    n_blocks: u64,
}

fn setup(n_fofs: usize) -> Setup {
    let mut rng: u64 = 0xdeadbeef_cafebabe;
    let mut t: u64 = 0;
    let mut start_times: Vec<u64> = (0..n_fofs)
        .map(|_| {
            t += (exp_sample(&mut rng) * MEAN_INTER_ARRIVAL).max(1.0) as u64;
            t
        })
        .collect();
    // drain_block_safe requires sorted order
    start_times.sort_unstable();

    let capacity = n_fofs.next_power_of_two();
    // start_times can span far beyond a small wheel's horizon; entries
    // beyond it are deferred in the ring buffer, not dropped, so a modest
    // N*D horizon (~65.5k samples @ D=256, N=256) is fine here.
    let (mut wheel_tx, wheel_rx) = time_wheel(capacity, 256, 256, capacity);
    let (_kill_tx, kill_rx) = kill_queue(64);

    for &s in &start_times {
        wheel_tx.push(standard_fof(s)).expect("queue full");
    }
    // Producer dropped — consumer still drains remaining items.

    let engine = RfofsEngine::new(
        SAMPLE_RATE,
        PanMode::Stereo,
        n_fofs,
        BLOCK_SIZE,
        vec![wheel_rx],
        kill_rx,
    );

    let last_start = *start_times.last().unwrap_or(&0);
    let total_samples = last_start + FOF_DURATION_SAMPLES + BLOCK_SIZE as u64;
    let n_blocks = total_samples.div_ceil(BLOCK_SIZE as u64);

    Setup { engine, n_blocks }
}

fn bench_process_block(c: &mut Criterion) {
    let mut group = c.benchmark_group("process_block");

    for &n_fofs in &[100usize, 500, 1000] {
        group.throughput(Throughput::Elements(n_fofs as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n_fofs), &n_fofs, |b, &n| {
            b.iter_batched(
                || setup(n),
                |mut s| {
                    let mut ch0 = vec![0.0f32; BLOCK_SIZE];
                    let mut ch1 = vec![0.0f32; BLOCK_SIZE];
                    for _ in 0..s.n_blocks {
                        ch0.fill(0.0);
                        ch1.fill(0.0);
                        s.engine.process_block(&mut [&mut ch0, &mut ch1], BLOCK_SIZE);
                    }
                    black_box((ch0, ch1))
                },
                BatchSize::LargeInput,
            )
        });
    }

    group.finish();
}

criterion_group!(benches, bench_process_block);
criterion_main!(benches);
