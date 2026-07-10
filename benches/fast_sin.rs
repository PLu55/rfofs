use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rfofs::fastsin::{fast_sin, fast_sin_quarter};

// Deterministic pseudo-random phases in [0, 1), generated once and reused so
// both variants do identical work — only the sin implementation differs.
fn xorshift64(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn phases(n: usize) -> Vec<f32> {
    let mut rng: u64 = 0x5eed_1234_abcd_ef01;
    (0..n)
        .map(|_| ((xorshift64(&mut rng) >> 11) as f64 / (1u64 << 53) as f64) as f32)
        .collect()
}

fn bench_sin(c: &mut Criterion) {
    let mut group = c.benchmark_group("sin");
    let n = 4096;
    let ps = phases(n);

    group.bench_function("std_sin", |b| {
        b.iter(|| {
            let mut acc = 0.0f32;
            for &p in &ps {
                acc += black_box(p * std::f32::consts::TAU).sin();
            }
            black_box(acc)
        })
    });

    group.bench_function("fast_sin_lut", |b| {
        b.iter(|| {
            let mut acc = 0.0f32;
            for &p in &ps {
                acc += fast_sin(black_box(p));
            }
            black_box(acc)
        })
    });

    group.bench_function("fast_sin_quarter_lut", |b| {
        b.iter(|| {
            let mut acc = 0.0f32;
            for &p in &ps {
                acc += fast_sin_quarter(black_box(p));
            }
            black_box(acc)
        })
    });

    group.finish();
}

criterion_group!(benches, bench_sin);
criterion_main!(benches);
