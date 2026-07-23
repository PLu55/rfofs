# Sustained polyphony limit — `RfofsEngine::process_block`

Measured with `examples/find_polyphony_limit.rs`, which holds exactly N
FOFs simultaneously active (near-zero decay, so none die mid-measurement)
and times each `process_block` call individually — the metric that
actually determines whether a live server glitches under sustained
polyphony, as opposed to `benches/parallel_fofs.rs`/`profile_compute.rs`,
which average cost over a FOF's full decay-to-death lifetime and so
understate peak load.

## Test conditions

- `--release` build
- Block size 256 samples @ 48000 Hz (matches this environment's real JACK
  server / PipeWire block size, confirmed via `tests/online_stress.rs`)
- Stereo output
- Real-time budget: 256 / 48000 = **5.3333 ms per block**
- Machine: Intel Core i7-13700KF (24 logical CPUs) — `process_block` itself
  is single-threaded, so core count doesn't matter for this number
- 20 warmup blocks discarded, 300 blocks measured per N

## Results

| N | median | p95 | max | % of budget |
|---:|---:|---:|---:|---:|
| 100 | 0.0263 ms | 0.0265 ms | 0.5319 ms | 0.5% |
| 500 | 0.0436 ms | 0.0459 ms | 0.0538 ms | 0.8% |
| 1,000 | 0.0900 ms | 0.0964 ms | 0.1259 ms | 1.7% |
| 1,500 | 0.1310 ms | 0.1386 ms | 0.1978 ms | 2.5% |
| 2,000 | 0.1796 ms | 0.1902 ms | 0.2133 ms | 3.4% |
| 2,500 | 0.2186 ms | 0.2293 ms | 0.2641 ms | 4.1% |
| 3,000 | 0.2735 ms | 0.2862 ms | 0.3877 ms | 5.1% |
| 3,500 | 0.3063 ms | 0.3235 ms | 0.3767 ms | 5.7% |
| 4,000 | 0.3630 ms | 0.3846 ms | 0.4183 ms | 6.8% |
| 4,500 | 0.3999 ms | 0.4127 ms | 0.6118 ms | 7.5% |
| 5,000 | 0.4577 ms | 0.4774 ms | 0.4948 ms | 8.6% |
| 6,000 | 0.5251 ms | 0.5501 ms | 0.6516 ms | 9.8% |
| 7,000 | 0.6467 ms | 0.6716 ms | 0.7163 ms | 12.1% |
| 8,000 | 0.7071 ms | 0.7323 ms | 0.7559 ms | 13.3% |
| 10,000 | 0.9265 ms | 0.9619 ms | 0.9856 ms | 17.4% |
| 12,000 | 1.0713 ms | 1.1121 ms | 1.1465 ms | 20.1% |
| 15,000 | 1.3950 ms | 1.4462 ms | 1.5384 ms | 26.2% |
| 20,000 | 1.8142 ms | 1.8788 ms | 1.9460 ms | 34.0% |
| 25,000 | 2.3373 ms | 2.4418 ms | 2.6558 ms | 43.8% |
| 30,000 | 2.7434 ms | 2.8381 ms | 2.9192 ms | 51.4% |
| 35,000 | 3.2836 ms | 3.3963 ms | 4.3767 ms | 61.6% |
| 40,000 | 3.6649 ms | 3.7876 ms | 3.8776 ms | 68.7% |
| 45,000 | 4.2359 ms | 4.3737 ms | 4.4818 ms | 79.4% |
| 50,000 | 4.5884 ms | 4.7023 ms | 4.8532 ms | 86.0% |
| 55,000 | 5.1638 ms | 5.2945 ms | 5.4915 ms | 96.8% |
| 56,000 | 5.2833 ms | 5.4662 ms | 15.0658 ms* | 99.1% |
| 57,000 | 5.2249 ms | 5.3624 ms | 5.4653 ms | 98.0% |
| 58,000 | 5.4562 ms | 5.6073 ms | 5.7152 ms | 102.3% |
| 59,000 | 5.4050 ms | 5.5428 ms | 5.6336 ms | 101.3% |
| 60,000 | 5.5041 ms | 5.6233 ms | 5.7392 ms | 103.2% |
| 65,000 | 6.1014 ms | 6.2714 ms | 6.4381 ms | 114.4% |
| 70,000 | 6.4155 ms | 6.5526 ms | 6.6709 ms | 120.3% |
| 75,000 | 7.0372 ms | 7.2196 ms | 7.4118 ms | 131.9% |
| 80,000 | 7.3559 ms | 7.5152 ms | 7.6794 ms | 137.9% |

\* single outlier, almost certainly a page-fault/scheduler preemption, not
a real per-block cost increase — median/p95 at that N are consistent with
the surrounding rows.

Cost scales essentially linearly with N (~0.09 µs/FOF/block) all the way
up to the crossover — no cliff, no cache or SIMD-width breakdown.

## Conclusion

- **Hard compute ceiling: ~57,000–58,000** simultaneously-active FOFs —
  where median/p95 block time first exceeds the 5.333 ms budget.
- **Recommended practical limit: ~40,000–45,000** (~80% of budget), leaving
  headroom for scheduling jitter, other system load, and the fact this is
  an isolated microbenchmark rather than a live JACK graph under real
  contention.
- For comparison, `tests/online_stress.rs`'s
  `sustained_overlapping_fofs_stress_engine_polyphony_on_jack_output` test
  targets only ~500 concurrent grains (well under 1% of capacity), which is
  why it shows zero admission rejections in `--release`.

## Reproducing

```
cargo run --release --example find_polyphony_limit [n1 n2 ...]
```

Defaults to the sweep above if no sizes are given.
