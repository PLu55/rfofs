//! Stress tests of the online JACK server path — two different failure
//! modes are targeted, deliberately, rather than the correctness/latency
//! checks in `tests/jack_server_fof.rs`:
//!
//! - `bursty_short_fofs_stress_admission_queue_on_jack_output`: ~1e4 short
//!   (~50ms) FOFs, pushed in bursts (not real-time-paced) with a shrunk
//!   wheel `slot_capacity`, to stress the shm ring / client API backpressure
//!   handling and deliberately provoke onset-admission rejections
//!   (`RejectReason::TooLate`/`SlotFull` in `src/queue.rs`).
//! - `sustained_overlapping_fofs_stress_engine_polyphony_on_jack_output`:
//!   ~1e4 longer (~1s) FOFs at a density tuned for heavy sustained overlap,
//!   to stress the engine's per-block synthesis/pan-scatter cost under high
//!   polyphony (`src/engine.rs`'s `Slab`-backed active-FOF pool).
//!
//! Both use a Poisson-process construction for onset timing: successive
//! inter-arrival gaps are drawn from an exponential distribution (mean =
//! 1/density) via a small seeded PRNG (no `rand`/`rand_distr` dependency —
//! deterministic and reproducible from a fixed seed), cumulative-summed
//! into strictly-increasing `start_sample` values — which also happens to
//! satisfy `queue.rs`'s "producers must submit FofParams in non-decreasing
//! start_sample order" contract for free.
//!
//! Like `tests/jack_server_fof.rs`, these are **not** hermetic: they
//! require a real JACK server already running. Unlike that file, both
//! tests here are `#[ignore]`d by default — each intentionally loads the
//! server harder and takes tens of seconds, so they'd slow down (and could
//! contend for the same real JACK server with) the default `cargo test`
//! suite. Run them explicitly:
//!
//!   cargo test --test online_stress -- --ignored
//!
//! `QueueStats`-based assertions (rejection counts, admission histogram)
//! only fire when the spawned `rfofs` binary was built with the
//! `statistics` feature (`cargo test`'s feature unification applies to
//! `CARGO_BIN_EXE_rfofs` too, since it's the same package):
//!
//!   cargo test --features statistics --test online_stress -- --ignored
//!
//! `sustained_overlapping_fofs_stress_engine_polyphony_on_jack_output`'s
//! low-rejection-rate check additionally needs an optimized build to be
//! meaningful: an unoptimized (`dev`) build's per-block synthesis for ~500
//! concurrently overlapping FOFs can genuinely miss JACK's real-time
//! deadline, producing xrun-driven rejections that reflect debug-build
//! performance rather than an engine regression — that specific check is
//! skipped (not failed) under `cfg!(debug_assertions)`. Run with `--release`
//! for it to actually validate anything:
//!
//!   cargo test --release --features statistics --test online_stress -- --ignored
//!
//! Without that feature, `stats_enabled()` is false and those checks are
//! skipped (logged, not failed) — the audio-domain assertions still run.

use std::ffi::CString;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rfofs::fof::FofParams;
use rfofs::shm::{ClientShm, SharedControlBlock, SHM_NAME_ENV};
use sndfile::{OpenOptions, ReadOptions, SndFileIO};

// ─────────────────────────────────────────────────────────────────────────────
// Harness plumbing — local copies of the patterns in tests/jack_server_fof.rs
// (can't be shared across test binaries without a tests/support/mod.rs).
// ─────────────────────────────────────────────────────────────────────────────

/// Both tests in this file drive the same real, shared JACK server, so they
/// must not run concurrently against it.
fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

/// Kills the wrapped child on drop, so a failing assertion (which unwinds
/// past any explicit cleanup) doesn't leak a JACK client / server process.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `shm_unlink`s the private control-plane segment on drop.
struct ShmCleanup(CString);

impl Drop for ShmCleanup {
    fn drop(&mut self) {
        unsafe { libc::shm_unlink(self.0.as_ptr()) };
    }
}

/// Polls `client`'s output ports matching `pattern` until at least two are
/// present that weren't in `existing` (i.e. belong to a newly-spawned
/// `rfofs`, however JACK ended up naming it if `rfofs` was already taken).
fn wait_for_new_ports(
    client: &jack::Client,
    pattern: &str,
    existing: &[String],
    timeout: Duration,
) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let found = client.ports(Some(pattern), None, jack::PortFlags::IS_OUTPUT);
        let new: Vec<String> = found.into_iter().filter(|p| !existing.contains(p)).collect();
        if new.len() >= 2 {
            return new;
        }
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for a new rfofs's output ports matching {pattern:?}; \
                 only found {new:?} beyond the pre-existing {existing:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Spawns a private `rfofs` against the real JACK server with a unique shm
/// segment, waits for its output ports to appear, and attaches a client to
/// its control plane. Returns everything the caller needs to keep alive for
/// the duration of the test (drop order: client_shm, then _server, then
/// _shm_cleanup, then harness_client — Rust drops fields/locals in reverse
/// declaration order, so callers should bind the returned tuple to named
/// locals rather than `_`).
fn spawn_stress_server(
    shm_name_prefix: &str,
    extra_args: &[&str],
) -> (KillOnDrop, ShmCleanup, jack::Client, String, ClientShm) {
    let shm_name = format!("{shm_name_prefix}_{}", std::process::id());
    let shm_cleanup = ShmCleanup(CString::new(shm_name.clone()).unwrap());
    // SAFETY: serialized by `serial_guard()` — no concurrent access to the
    // process environment from other tests in this binary.
    unsafe { std::env::set_var(SHM_NAME_ENV, &shm_name) };

    let (harness_client, _status) = jack::Client::new(
        &format!("rfofs_test_harness_{shm_name_prefix}"),
        jack::ClientOptions::NO_START_SERVER,
    )
    .expect("failed to open JACK client — is a JACK server running?");
    let existing_ports =
        harness_client.ports(Some("^rfofs.*:out_.*$"), None, jack::PortFlags::IS_OUTPUT);

    // Piped (not null) stdin: main.rs blocks on a final `read_line` to stay
    // alive, and closing/EOF-ing that would make it exit immediately.
    let server = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_rfofs"))
            .env(SHM_NAME_ENV, &shm_name)
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn rfofs server"),
    );

    let mut new_ports = wait_for_new_ports(
        &harness_client,
        "^rfofs.*:out_.*$",
        &existing_ports,
        Duration::from_secs(10),
    );
    new_ports.sort();
    let client_name = new_ports[0].split(':').next().unwrap().to_string();

    let client_shm = ClientShm::attach().expect("failed to attach to rfofs control plane");

    (server, shm_cleanup, harness_client, client_name, client_shm)
}

// ─────────────────────────────────────────────────────────────────────────────
// PRNG + exponential (Poisson-process) sampling
// ─────────────────────────────────────────────────────────────────────────────

/// A tiny seeded PRNG (SplitMix64) — deterministic and dependency-free, so
/// stress runs are reproducible from a fixed seed without pulling in
/// `rand`/`rand_distr`.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform sample in the open interval (0, 1) — never 0 (so `ln` below
    /// never sees 0) and never 1.
    fn next_open01(&mut self) -> f64 {
        // 53 bits of mantissa, offset by 1 to land in (0, 2^53]/2^53 = (0, 1].
        // Then reflect a possible 1.0 back into the open interval.
        let bits = self.next_u64() >> 11; // top 53 bits
        let u = ((bits as f64) + 1.0) / ((1u64 << 53) as f64 + 1.0);
        u
    }
}

/// Draws one inter-arrival gap (in samples) from an exponential distribution
/// with the given mean, via inverse-CDF sampling. `mean_gap_samples` is
/// `sample_rate / density_hz`.
fn exponential_gap_samples(rng: &mut SplitMix64, mean_gap_samples: f64) -> u64 {
    let u = rng.next_open01();
    let gap = -mean_gap_samples * (1.0 - u).ln();
    gap.max(1.0) as u64
}

// ─────────────────────────────────────────────────────────────────────────────
// Stress configuration
// ─────────────────────────────────────────────────────────────────────────────

/// All the "easy to retune" knobs for a stress run. `density_hz` can't be a
/// compile-time constant — it's derived from the spawned server's *real*
/// `block_size`/`sample_rate`, only known after `ClientShm::attach()`
/// succeeds — so callers compute it with a `let` right after attaching and
/// fold it into this struct afterwards.
struct StressConfig {
    /// Total number of FOFs to submit.
    n_fofs: u64,
    /// Target audible duration of each FOF, in seconds (drives `alpha`).
    fof_duration_secs: f64,
    /// Mean onset arrival rate (events/sec) of the exponential inter-arrival
    /// process.
    density_hz: f64,
    /// PRNG seed — fixed per test for reproducibility.
    seed: u64,
    /// How far ahead of the server's live `current_sample()` the first
    /// onset is scheduled.
    lead_secs: f64,
    /// Pacer's forward-lookahead cap, as a fraction of the wheel's horizon
    /// (`n_slots * block_size` samples). Close to 1.0 lets the pusher race
    /// far ahead of real time (bursty); small values keep it tracking near
    /// real time throughout (sustained).
    lookahead_fraction_of_horizon: f64,
    amp_min: f32,
    amp_max: f32,
    azm_min: f32,
    azm_max: f32,
    /// Attack duration, seconds.
    beta: f32,
    /// Envelope floor (linear) that `fof_duration_secs` is tuned to reach.
    fade_level: f32,
    /// Fade-out ramp duration, seconds.
    fade_dur: f32,
    max_push_retries: u32,
    base_backoff: Duration,
    /// Passed to the spawned `rfofs` as `--n-slots`, and used locally for
    /// horizon math — kept explicit so the test's math can't silently drift
    /// from `main.rs`'s defaults if those ever change.
    n_slots: usize,
    /// Passed to the spawned `rfofs` as `--slot-capacity`.
    slot_capacity: usize,
}

impl StressConfig {
    fn alpha(&self) -> f32 {
        -self.fade_level.ln() / (self.fof_duration_secs as f32 - self.beta)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Push-with-retry + horizon-aware pacer
// ─────────────────────────────────────────────────────────────────────────────

/// Submits `params`, retrying with exponential backoff if the shm ring
/// (`FOF_CAP = 4096`) is momentarily full. This bounded-retry handling of
/// backpressure is itself part of what's being stress-tested — the "client
/// API" side of a bursty submitter, in contrast to the bare `.expect()`
/// existing correctness tests use (where the ring is never under real
/// pressure).
fn push_with_retry(block: &SharedControlBlock, mut params: FofParams, cfg: &StressConfig) {
    for attempt in 0..cfg.max_push_retries {
        match block.try_push_fof(params) {
            Ok(()) => return,
            Err(rejected) => {
                params = rejected;
                let backoff = cfg.base_backoff * (1u32 << attempt.min(6)); // caps at 64x base
                std::thread::sleep(backoff);
            }
        }
    }
    panic!(
        "shm FOF ring stayed full across {} retries (id={} start_sample={}) — \
         bridging thread appears stuck",
        cfg.max_push_retries, params.id, params.start_sample
    );
}

/// Blocks until `start_sample` is within `lookahead_samples` of the
/// server's live clock (push-now), sleeping and re-polling otherwise
/// (sleep-and-retry). Panics with a diagnostic if the server's clock
/// appears stalled for longer than `stall_timeout` — distinguishes "server
/// genuinely can't keep up" from a test bug.
fn wait_until_within_lookahead(
    block: &SharedControlBlock,
    start_sample: u64,
    lookahead_samples: u64,
    stall_timeout: Duration,
) {
    let deadline = Instant::now() + stall_timeout;
    loop {
        let now = block.current_sample();
        if start_sample <= now.saturating_add(lookahead_samples) {
            return;
        }
        if Instant::now() > deadline {
            panic!(
                "server clock stalled: still {} samples ahead of current_sample()={} \
                 after {:?} — is rfofs alive?",
                start_sample - now,
                now,
                stall_timeout
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Builds one grain's params. Always fire-and-forget (`id: 0`) — this
/// stress run never tracks or individually kills grains, so there's no need
/// for unique nonzero ids (see the `FofParams::id` invariant in `fof.rs`).
fn make_fof_params(cfg: &StressConfig, rng: &mut SplitMix64, start_sample: u64) -> FofParams {
    let freqs = [261.63f32, 293.66, 329.63, 392.00, 440.00, 523.25]; // C4 D4 E4 G4 A4 C5
    let amp = cfg.amp_min + (cfg.amp_max - cfg.amp_min) * rng.next_open01() as f32;
    let azm = cfg.azm_min + (cfg.azm_max - cfg.azm_min) * rng.next_open01() as f32;
    let f = freqs[(rng.next_u64() as usize) % freqs.len()];
    FofParams {
        id: 0,
        start_sample,
        f,
        gliss: 0.0,
        phi: 0.0,
        amp,
        alpha: cfg.alpha(),
        beta: cfg.beta,
        fade_level: cfg.fade_level,
        fade_dur: cfg.fade_dur,
        azm,
        elev: 0.0,
        distance: 1.0,
    }
}

/// Runs the exponential-arrival push loop: draws `cfg.n_fofs` inter-arrival
/// gaps, paces each push against the wheel's horizon via
/// `wait_until_within_lookahead`, and submits via `push_with_retry`.
fn run_stress_push_loop(block: &SharedControlBlock, cfg: &StressConfig) {
    let sample_rate = block.sample_rate() as f64;
    let block_size = block.block_size() as u64;
    let horizon_samples = cfg.n_slots as u64 * block_size;
    let lookahead_samples = (horizon_samples as f64 * cfg.lookahead_fraction_of_horizon) as u64;
    let mean_gap_samples = sample_rate / cfg.density_hz;

    let mut rng = SplitMix64::new(cfg.seed);
    let t0 = block.current_sample() + (sample_rate * cfg.lead_secs) as u64;
    let mut cum_offset = 0u64;

    for _ in 0..cfg.n_fofs {
        let gap = exponential_gap_samples(&mut rng, mean_gap_samples);
        cum_offset += gap;
        let start_sample = t0 + cum_offset;

        wait_until_within_lookahead(block, start_sample, lookahead_samples, Duration::from_secs(5));

        let params = make_fof_params(cfg, &mut rng, start_sample);
        push_with_retry(block, params, cfg);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Assertions
// ─────────────────────────────────────────────────────────────────────────────

fn assert_finite_and_bounded(samples: &[f32], peak_ceiling: f32) {
    assert!(!samples.is_empty(), "recorded wav should contain samples");
    assert!(
        samples.iter().all(|s| s.is_finite()),
        "recorded audio contains a non-finite sample (NaN/Inf) — likely synthesis blow-up under load"
    );
    let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    assert!(peak > 0.01, "expected audible activity, got peak amplitude {peak}");
    assert!(
        peak < peak_ceiling,
        "peak amplitude {peak} exceeds sanity ceiling {peak_ceiling} — possible runaway summation"
    );
}

/// Windowed activity-presence scan over `[start_frame, end_frame)`: every
/// `window_frames`-sized window must contain at least one sample above
/// `threshold` on some channel. Same style as
/// `continuous_fofs_produce_signal_throughout_on_jack_output` in
/// `tests/jack_server_fof.rs`.
fn assert_activity_present(
    samples: &[f32],
    n_channels: usize,
    sample_rate: f32,
    window_frames: usize,
    threshold: f32,
    start_frame: usize,
    end_frame: usize,
) {
    let mut frame = start_frame;
    while frame < end_frame {
        let window_end = (frame + window_frames).min(end_frame);
        let has_signal = (frame..window_end)
            .any(|fr| (0..n_channels).any(|ch| samples[fr * n_channels + ch].abs() > threshold));
        assert!(
            has_signal,
            "silence detected in [{:.3}s, {:.3}s)",
            frame as f32 / sample_rate,
            window_end as f32 / sample_rate,
        );
        frame = window_end;
    }
}

struct QueueStatsSnapshot {
    too_late: u64,
    too_early: u64,
    slot_full: u64,
    admitted: u64,
}

/// Reads `QueueStats` off the shared control block if the spawned server
/// was built with the `statistics` feature; otherwise returns `None` and
/// the caller should skip stats-based assertions entirely (logged, not
/// failed) rather than treat an unbuilt feature as a bug.
fn read_queue_stats(block: &SharedControlBlock) -> Option<QueueStatsSnapshot> {
    if !block.stats_enabled() {
        println!(
            "stats_enabled() == false (rfofs built without --features statistics) — \
             skipping QueueStats assertions"
        );
        return None;
    }
    let stats = &block.stats;
    let admitted: u64 = stats
        .slot_offset_histogram
        .iter()
        .map(|b| b.load(Ordering::Relaxed))
        .sum();
    Some(QueueStatsSnapshot {
        too_late: stats.too_late.load(Ordering::Relaxed),
        too_early: stats.too_early.load(Ordering::Relaxed),
        slot_full: stats.slot_full.load(Ordering::Relaxed),
        admitted,
    })
}

/// Coherence + scenario-specific sanity checks on the admission stats,
/// gated on the server having been built with `statistics`.
fn assert_queue_stats_sane(block: &SharedControlBlock, n_fofs: u64, expect_rejections: bool) {
    let Some(snap) = read_queue_stats(block) else { return };

    println!(
        "QueueStats: admitted={} too_late={} too_early={} slot_full={}",
        snap.admitted, snap.too_late, snap.too_early, snap.slot_full
    );

    assert_eq!(
        snap.too_early, 0,
        "too_early should be structurally unreachable via drain_block_safe's admit_before \
         pre-filter (see queue.rs) — a nonzero count means that invariant has changed"
    );

    // Upper-bound, not equality: main.rs's bridging thread silently drops an
    // entry if the wheel's ingress ring is ever full (`let _ =
    // wheel_tx.push(p)`), with no counter — so exact accounting isn't
    // guaranteed under heavy load.
    let accounted = snap.admitted + snap.too_late + snap.slot_full;
    assert!(
        accounted <= n_fofs,
        "admitted+rejected ({accounted}) exceeds n_fofs ({n_fofs}) — impossible, something double-counted"
    );

    if expect_rejections {
        assert!(
            snap.too_late + snap.slot_full > 0,
            "expected the deliberately bursty load to provoke at least some admission \
             rejections (too_late/slot_full); got none — the burst wasn't actually stressing anything"
        );
    } else if cfg!(debug_assertions) {
        // An unoptimized build's per-block synthesis for hundreds of
        // concurrently overlapping FOFs can genuinely fail to keep up with
        // JACK's real-time deadline, producing xrun-driven `too_late`
        // rejections that reflect debug-build performance, not an engine
        // bug — see the module doc comment (`--release` required for this
        // check to be meaningful). Skip rather than weaken the bound, so a
        // real `--release` regression still fails loudly.
        println!(
            "debug (non-release) build: skipping the low-rejection-rate check — \
             got {} too_late + {} slot_full out of {n_fofs}, not held against a debug build",
            snap.too_late, snap.slot_full
        );
    } else {
        let rejected = snap.too_late + snap.slot_full;
        assert!(
            (rejected as f64) <= 0.01 * n_fofs as f64,
            "expected admission rejections to stay rare (<=1% of n_fofs) for this \
             non-bursty scenario; got {rejected} out of {n_fofs}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared recording helper
// ─────────────────────────────────────────────────────────────────────────────

/// Spawns `jack_probe` recording `client_name`'s output ports for
/// `probe_duration_secs`, runs `push_loop` (expected to submit onsets while
/// the probe is recording), waits for the probe to finish, and returns the
/// recorded samples (interleaved) plus the channel count.
fn record_while_pushing(
    client_name: &str,
    probe_duration_secs: f64,
    wav_tag: &str,
    push_loop: impl FnOnce(),
) -> (Vec<f32>, usize) {
    let wav_path =
        format!("{}/jack_probe_{}_{}.wav", env!("CARGO_TARGET_TMPDIR"), wav_tag, std::process::id());

    let probe = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_jack_probe"))
            .args([
                "--pattern",
                &format!("{client_name}:out_.*"),
                "--count",
                "2",
                "--duration",
                &probe_duration_secs.to_string(),
                "--output",
                &wav_path,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn jack_probe"),
    );

    // Let jack_probe finish registering + connecting its input ports before
    // pushing starts, so recording has already started by the time the
    // first onset can possibly fire.
    std::thread::sleep(Duration::from_millis(300));

    push_loop();

    // jack_probe exits on its own once --duration elapses.
    let mut probe = probe;
    let status = probe.0.wait().expect("failed to wait on jack_probe");
    assert!(status.success(), "jack_probe exited with {status}");

    let mut snd = OpenOptions::ReadOnly(ReadOptions::Auto)
        .from_path(&wav_path)
        .expect("failed to open jack_probe's recorded wav");
    let n_channels = snd.get_channels();
    assert_eq!(n_channels, 2);
    let samples: Vec<f32> =
        SndFileIO::<f32>::read_all_to_vec(&mut snd).expect("failed to read recorded samples");
    let _ = std::fs::remove_file(&wav_path);

    (samples, n_channels)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Admission-queue / client-API stress: ~1e4 short (~50ms) FOFs with a
/// shrunk wheel `slot_capacity` and an aggressive pacer lookahead, so the
/// push loop bursts well ahead of the server's real-time clock rather than
/// pacing 1:1 with it — deliberately provoking admission rejections
/// (SlotFull/TooLate) and shm-ring backpressure, while checking the server
/// survives and keeps producing sane audio throughout.
#[test]
#[ignore]
fn bursty_short_fofs_stress_admission_queue_on_jack_output() {
    let _serial = serial_guard();

    let cfg_n_slots = 256;
    let cfg_slot_capacity = 8; // shrunk from the CLI default (64) to make SlotFull easy to trigger

    let (_server, _shm_cleanup, _harness_client, client_name, client_shm) = spawn_stress_server(
        "/rfofs_ctl_test_stress_bursty",
        &[
            "--n-slots",
            &cfg_n_slots.to_string(),
            "--slot-capacity",
            &cfg_slot_capacity.to_string(),
        ],
    );
    let block = client_shm.block();
    let sample_rate = block.sample_rate();
    let slot_duration_secs = block.block_size() as f64 / sample_rate as f64;

    // Target well above slot_capacity so overflow is routine, not tail variance.
    let target_events_per_slot = 24.0;
    let density_hz = target_events_per_slot / slot_duration_secs;

    let cfg = StressConfig {
        n_fofs: 10_000,
        fof_duration_secs: 0.05,
        density_hz,
        seed: 0x5EED_0001,
        lead_secs: 0.05,
        lookahead_fraction_of_horizon: 0.9,
        amp_min: 0.2,
        amp_max: 0.5,
        azm_min: -1.0,
        azm_max: 1.0,
        beta: 0.001,
        fade_level: 0.001,
        fade_dur: 0.005,
        max_push_retries: 20,
        base_backoff: Duration::from_micros(200),
        n_slots: cfg_n_slots,
        slot_capacity: cfg_slot_capacity,
    };

    let virtual_timeline_secs = cfg.n_fofs as f64 / cfg.density_hz;
    let probe_duration_secs = virtual_timeline_secs + cfg.fof_duration_secs * 4.0 + 1.0;
    println!(
        "bursty_short_fofs: density_hz={:.1} slot_capacity={} virtual_timeline_secs={:.2} probe_duration_secs={:.2}",
        cfg.density_hz, cfg.slot_capacity, virtual_timeline_secs, probe_duration_secs
    );

    let (samples, n_channels) =
        record_while_pushing(&client_name, probe_duration_secs, "stress_bursty", || {
            run_stress_push_loop(block, &cfg);
        });

    assert_finite_and_bounded(&samples, 20.0);

    let n_frames = samples.len() / n_channels;
    // Leading margin must clear jack_probe's own registration wait (300ms,
    // see record_while_pushing) plus this scenario's lead_secs headroom
    // before the first onset can possibly be audible — a fixed 0.5s margin
    // is wrong once lead_secs alone approaches or exceeds that.
    let start_margin_secs = 0.3 + cfg.lead_secs + 0.2;
    let start_frame = ((sample_rate as f64 * start_margin_secs) as usize).min(n_frames);
    // Only check activity across the actual push window (the recording
    // runs deliberately longer than that — see probe_duration_secs above —
    // to capture the last grain's decay tail, which legitimately trails off
    // into silence and isn't part of what this check is validating).
    let end_margin_frames = (sample_rate as usize) / 2; // 0.5s
    let push_frames = (virtual_timeline_secs * sample_rate as f64) as usize;
    let end_frame = (start_frame + push_frames)
        .saturating_sub(end_margin_frames)
        .min(n_frames)
        .max(start_frame);

    // Looser window/threshold than a strict continuity check: deliberate
    // local rejections can legitimately thin out coverage — this is a
    // coarse "the stress didn't kill the signal entirely" check.
    let window_frames = (sample_rate as usize) / 2; // 500ms
    assert_activity_present(&samples, n_channels, sample_rate, window_frames, 1e-3, start_frame, end_frame);

    assert_queue_stats_sane(block, cfg.n_fofs, true);
}

/// Engine-polyphony stress: ~1e4 longer (~1s) FOFs at a density tuned (via
/// Little's-law reasoning: expected concurrency ~= density_hz *
/// fof_duration_secs) for heavy sustained overlap, paced close to real time
/// throughout — stressing per-block synthesis/pan-scatter cost under high
/// polyphony rather than admission-queue turnover.
#[test]
#[ignore]
fn sustained_overlapping_fofs_stress_engine_polyphony_on_jack_output() {
    let _serial = serial_guard();

    let cfg_n_slots = 256;
    let cfg_slot_capacity = 64; // CLI default — this scenario isn't trying to trip admission

    let (_server, _shm_cleanup, _harness_client, client_name, client_shm) =
        spawn_stress_server("/rfofs_ctl_test_stress_polyphony", &[]);
    let block = client_shm.block();
    let sample_rate = block.sample_rate();

    let concurrency_target = 500.0; // expected number of simultaneously-active grains
    let fof_duration_secs = 1.0;
    let density_hz = concurrency_target / fof_duration_secs; // Little's law: L = lambda * W

    let cfg = StressConfig {
        n_fofs: 10_000,
        fof_duration_secs,
        density_hz,
        seed: 0x5EED_0002,
        lead_secs: 0.5,
        // Small, near-lead_secs lookahead cap: pacer tracks close to real
        // time throughout instead of front-loading the whole burst.
        lookahead_fraction_of_horizon: 0.05,
        // ~500-way concurrent summation would blow past a sane peak with
        // typical single-grain amplitudes, so keep per-grain amp low.
        amp_min: 0.02,
        amp_max: 0.05,
        azm_min: -1.0,
        azm_max: 1.0,
        beta: 0.02,
        fade_level: 0.001,
        fade_dur: 0.05,
        max_push_retries: 20,
        base_backoff: Duration::from_micros(200),
        n_slots: cfg_n_slots,
        slot_capacity: cfg_slot_capacity,
    };

    let virtual_timeline_secs = cfg.n_fofs as f64 / cfg.density_hz;
    let probe_duration_secs = virtual_timeline_secs + cfg.fof_duration_secs * 3.0 + 0.5;
    println!(
        "sustained_overlapping: density_hz={:.1} virtual_timeline_secs={:.2} probe_duration_secs={:.2}",
        cfg.density_hz, virtual_timeline_secs, probe_duration_secs
    );

    let (samples, n_channels) =
        record_while_pushing(&client_name, probe_duration_secs, "stress_polyphony", || {
            run_stress_push_loop(block, &cfg);
        });

    assert_finite_and_bounded(&samples, 20.0);

    let n_frames = samples.len() / n_channels;
    // See the comments on the equivalent lines in the bursty-scenario test:
    // the leading margin must clear jack_probe's registration wait plus
    // this scenario's (larger) lead_secs headroom, and the trailing margin
    // is bounded to the actual push window rather than the whole recording
    // (which deliberately runs longer, to capture the last grain's tail).
    let start_margin_secs = 0.3 + cfg.lead_secs + 0.2;
    let start_frame = ((sample_rate as f64 * start_margin_secs) as usize).min(n_frames);
    let end_margin_frames = (sample_rate as usize) / 2; // 0.5s
    let push_frames = (virtual_timeline_secs * sample_rate as f64) as usize;
    let end_frame = (start_frame + push_frames)
        .saturating_sub(end_margin_frames)
        .min(n_frames)
        .max(start_frame);

    // Strict window, matching the existing continuous test: with 500-way
    // sustained overlap, any real gap in the trimmed steady-state region
    // indicates a genuine dropout, not an expected rejection artifact.
    let window_frames = (sample_rate as usize) / 10; // 100ms
    assert_activity_present(&samples, n_channels, sample_rate, window_frames, 1e-3, start_frame, end_frame);

    assert_queue_stats_sane(block, cfg.n_fofs, false);
}
