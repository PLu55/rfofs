# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

RFofs is a real-time FOF (Formant-Wave-Function) granular audio synthesizer written in Rust. `src/main.rs` runs it as a live JACK server driven entirely by external clients over a shared-memory control plane (see `rfofs-client`); the same core engine also drives an offline WAV renderer (`OfflineRenderer`) used directly by library callers and by the integration test.

## Commands

- Build: `cargo build --release`
- Type-check only: `cargo check`
- Test: `cargo test` (unit tests live inline in `src/pan.rs` and `src/queue.rs` under `#[cfg(test)] mod tests`; `tests/offline_render.rs` is an integration test of the offline WAV-rendering path)
- Run a single test: `cargo test <test_name>`, e.g. `cargo test wheel_rejects_event_beyond_horizon`
- Run the server (JACK mode): `cargo run --release` — then drive it externally via `rfofs-client` (see `rfofs-client/examples/shm_client_smoke.rs`)
- Benchmarks (Criterion, `harness = false`): `cargo bench --bench process_block` and `cargo bench --bench parallel_fofs`

## Architecture

`src/lib.rs` exposes five modules: `engine`, `fof`, `offline`, `pan`, `queue`.

- **`src/fof.rs`** — `FofParams` is a `Copy`, heap-free struct sent across threads through the queues (`id == 0` means fire-and-forget; nonzero ids are trackable/killable). `FofState` is the runtime per-grain state held in the engine's slab, driven by a `FofPhase` state machine: `Attack → Decay → FadeOut → Dead`. Envelope shape is a half-cosine sigmoid attack times exponential decay, normalised via the `fof_amax()` polynomial fit so `params.amp` is always the true peak regardless of `alpha`/`beta`.
- **`src/queue.rs`** — the cross-thread transport layer. `Wheel<T>` is a single-level timing wheel (`N` slots × `D` samples, `M` capacity per slot) used for lock-free scheduling of upcoming FOF onsets. `TimeWheelProducer`/`TimeWheelConsumer` wrap it around an `rtrb` SPSC ring buffer; **producers must push `FofParams` in non-decreasing `start_sample` order** — `drain_block_safe` stops draining the ring buffer at the first not-yet-ready entry, relying on that ordering rather than doing a general priority-queue admission. `kill_queue` is a simpler SPSC ring buffer of `FofKillRequest` for early fade-out by id.
- **`src/engine.rs`** — `RfofsEngine` is the audio-thread-owned core and must never be shared across threads; all cross-thread communication goes exclusively through the `queue.rs` structures. Active FOFs live in a `Slab<FofState>` plus an `id → slab index` `HashMap` (only for `id != 0`). `process_block()` runs the per-block pipeline: drain wheels/kills → spawn new FOFs → apply kill requests → synth each active FOF into a reused mono scratch buffer → pan-scatter into per-channel outputs → reap `Dead` FOFs → advance `sample_clock`. All scratch buffers (`mono_buf`, `gains_buf`, `active_keys`, `dead_keys`) are pre-allocated and reused every block — no allocation on the audio-thread hot path.
- **`src/pan.rs`** — `PanMode` (`Mono` / `Stereo` / `Ambisonic { order, dims, reverb }`), parsed from `"amb OnDmRk"`-style notation strings (e.g. `"amb O2 D3 R1"`). `pan_gains()` computes static per-channel gains once per FOF per block (not per-sample) from azimuth/elevation/distance. Ambisonic encoding currently only implements order 1; order ≥ 2 logs a warning and falls back to order-1 encoding (stub).
- **`src/offline.rs`** — `OfflineRenderer` drives the same `RfofsEngine`/queue machinery as the JACK path but pulls blocks synchronously instead of from an audio callback, writing 32-bit float WAV via the `sndfile` crate. `add_fof()` requires weakly-monotonic `start_sample` (enforced by an `assert!`) and renders blocks on demand to stay just behind the newest FOF's start time; `close()` drains remaining audio until all FOFs reach `Dead` (30 s safety cap).
- **`src/main.rs`** — entry point; runs the JACK server only. Wires up the engine, queues, and a `ServerShm` control-plane segment, then spawns a non-realtime bridging thread that forwards FOF/kill requests arriving from external clients (e.g. `rfofs-client`) into the same `TimeWheelProducer`/`KillQueueProducer` the engine's audio callback was constructed with. It submits no FOFs of its own — all onsets come from connected clients. The offline-rendering demo that used to live here (`cargo run --release -- output.wav`) is now `tests/offline_render.rs`.

## Key invariants to preserve

- `RfofsEngine` and its scratch buffers are single-threaded and audio-thread-only. All cross-thread handoff must go through `queue.rs`'s lock-free structures, and `process_block()` must stay allocation-free.
- Callers of `TimeWheelProducer` must submit `FofParams` in non-decreasing `start_sample` order — the consumer's admission logic assumes this and will silently defer (not reject) an out-of-order/late entry rather than reordering it.
- `FofParams::id == 0` means fire-and-forget (untracked); nonzero ids must be unique among concurrently active FOFs to be individually killable via the kill queue.
