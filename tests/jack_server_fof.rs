//! End-to-end integration test of the online JACK server path: spawns the
//! real `rfofs` binary, connects `jack_probe` (`src/bin/jack_probe.rs`) to
//! its output ports, submits a FOF over the shared-memory control plane
//! (`rfofs::shm`, the same interface `rfofs-client` uses), and checks that
//! the recorded audio is silent before the onset and audible after it.
//!
//! Unlike `tests/offline_render.rs`, these tests are **not** hermetic: they
//! require a real JACK server already running in the test environment
//! (`jackd`/`jackdbus`/pipewire-jack), since neither `rfofs` nor `jack_probe`
//! start one themselves (`jack::ClientOptions::NO_START_SERVER`). Each run
//! uses a private, uniquely-scoped `RFOFS_SHM_NAME` (see
//! `rfofs::shm::SHM_NAME_ENV`) so it never steals the control plane away
//! from another `rfofs` instance that happens to already be running against
//! the same JACK server.
//!
//! `--clock-mode transport` additionally drives the engine's sample clock
//! from the *shared* JACK transport position (`src/clock.rs`), which only
//! advances while transport is rolling — so that variant starts/stops the
//! JACK transport itself around the test. Since transport is global server
//! state (not scoped to one client), `SERIAL_TESTS` below serializes all
//! tests in this file rather than letting cargo run them concurrently.

use std::ffi::CString;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rfofs::fof::FofParams;
use rfofs::shm::{ClientShm, SHM_NAME_ENV};
use sndfile::{OpenOptions, ReadOptions, SndFileIO};

/// All tests in this file drive the same real, shared JACK server (ports,
/// and — for the transport variant — the global transport state), so they
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

/// `shm_unlink`s the private control-plane segment on drop, so repeated test
/// runs don't accumulate stale segments in `/dev/shm`.
struct ShmCleanup(CString);

impl Drop for ShmCleanup {
    fn drop(&mut self) {
        unsafe { libc::shm_unlink(self.0.as_ptr()) };
    }
}

/// Stops the JACK transport on drop — used only by the transport-clock-mode
/// test, so it doesn't leave the shared, system-wide transport rolling for
/// other JACK clients after the test ends (pass or fail).
struct StopTransportOnDrop<'a>(&'a jack::Client);

impl Drop for StopTransportOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.transport().stop();
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

/// Spawns a private `rfofs` + `jack_probe` pair against the real JACK
/// server, submits one FOF onset partway through the recording, and returns
/// the probed samples (interleaved, 2 channels) for the caller to assert on.
///
/// `extra_args` is passed straight through to the `rfofs` binary (e.g.
/// `&["--clock-mode", "transport"]`); `roll_transport` additionally starts
/// the shared JACK transport once the server is up (required for
/// `--clock-mode transport`'s sample clock to advance at all) and stops it
/// again on return/panic via `StopTransportOnDrop`. `switch_clock_mode_to`,
/// if set, calls `SharedControlBlock::set_clock_mode` right after attaching
/// — used to prove a *live* client-issued switch (as opposed to the
/// `--clock-mode` startup flag) actually redirects the server's per-block
/// sample clock source.
fn run_fof_smoke_test(
    extra_args: &[&str],
    roll_transport: bool,
    switch_clock_mode_to: Option<u32>,
) -> Vec<f32> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let _serial = serial_guard();

    let shm_name = format!(
        "/rfofs_ctl_test_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let _shm_cleanup = ShmCleanup(CString::new(shm_name.clone()).unwrap());
    // SAFETY: serialized by `serial_guard()` above — no concurrent access to
    // the process environment from other tests in this binary.
    unsafe { std::env::set_var(SHM_NAME_ENV, &shm_name) };

    // Harness-only JACK client, used purely to enumerate ports and (for the
    // transport variant) drive transport — never activated, so it doesn't
    // participate in audio processing itself.
    let (harness_client, _status) =
        jack::Client::new("rfofs_test_harness", jack::ClientOptions::NO_START_SERVER)
            .expect("failed to open JACK client — is a JACK server running?");
    let existing_ports =
        harness_client.ports(Some("^rfofs.*:out_.*$"), None, jack::PortFlags::IS_OUTPUT);

    // Piped (not null) stdin: main.rs blocks on a final `read_line` to stay
    // alive, and closing/EOF-ing that would make it exit immediately.
    let _server = KillOnDrop(
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
    let sample_rate = client_shm.block().sample_rate();

    if let Some(mode) = switch_clock_mode_to {
        assert!(
            client_shm.block().set_clock_mode(mode),
            "set_clock_mode should accept a known clock-mode constant"
        );
        assert_eq!(client_shm.block().clock_mode(), mode);
    }

    // Only stops transport (shared, system-wide state) once we actually
    // started it — an unconditional guard would stop it even for the
    // frame-time test, which has no business touching it.
    let _stop_transport = if roll_transport {
        harness_client.transport().locate(0).expect("failed to locate transport");
        harness_client.transport().start().expect("failed to start transport");
        Some(StopTransportOnDrop(&harness_client))
    } else {
        None
    };

    let wav_path = format!(
        "{}/jack_probe_fof_{}.wav",
        env!("CARGO_TARGET_TMPDIR"),
        std::process::id()
    );
    let probe_duration_secs = 2.0f64;
    let onset_offset_secs = 0.5f64;

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

    // Let jack_probe finish registering + connecting its input ports (and,
    // for the transport variant, let the transport actually start rolling)
    // before scheduling the onset, so recording has already started by the
    // time it fires and there's a clean silent stretch beforehand to
    // contrast against.
    std::thread::sleep(Duration::from_millis(300));

    let start_sample =
        client_shm.block().current_sample() + (sample_rate as f64 * onset_offset_secs) as u64;
    client_shm
        .block()
        .try_push_fof(FofParams {
            id: 0,
            start_sample,
            f: 440.0,
            gliss: 0.0,
            phi: 0.0,
            amp: 0.8,
            alpha: 10.0, // ~0.7s decay
            beta: 0.01,  // 10ms attack
            fade_level: 0.001,
            fade_dur: 0.01,
            azm: 0.0,
            elev: 0.0,
            distance: 1.0,
        })
        .expect("fof request ring full");

    // jack_probe exits on its own once --duration elapses.
    let mut probe = probe;
    let status = probe.0.wait().expect("failed to wait on jack_probe");
    assert!(status.success(), "jack_probe exited with {status}");

    let mut snd = OpenOptions::ReadOnly(ReadOptions::Auto)
        .from_path(&wav_path)
        .expect("failed to open jack_probe's recorded wav");
    assert_eq!(snd.get_channels(), 2);
    let samples: Vec<f32> =
        SndFileIO::<f32>::read_all_to_vec(&mut snd).expect("failed to read recorded samples");
    let _ = std::fs::remove_file(&wav_path);
    samples
}

/// Asserts near-silence in the first quarter of `samples` (well before the
/// onset even allowing for probe startup jitter) and a clearly audible peak
/// somewhere over the whole recording.
fn assert_silence_then_onset(samples: &[f32]) {
    assert!(!samples.is_empty(), "recorded wav should contain samples");
    let n_channels = 2usize;
    let n_frames = samples.len() / n_channels;

    let quiet_frames = n_frames / 4;
    let quiet_peak = samples[..quiet_frames * n_channels]
        .iter()
        .fold(0.0f32, |m, &s| m.max(s.abs()));
    assert!(
        quiet_peak < 0.01,
        "expected near-silence before the FOF onset, got peak amplitude {quiet_peak}"
    );

    let overall_peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    assert!(
        overall_peak > 0.05,
        "expected an audible FOF onset, got peak amplitude {overall_peak}"
    );
}

#[test]
fn fof_onset_is_audible_on_jack_output() {
    let samples = run_fof_smoke_test(&[], false, None);
    assert_silence_then_onset(&samples);
}

#[test]
fn fof_onset_is_audible_with_transport_clock_mode() {
    let samples = run_fof_smoke_test(&["--clock-mode", "transport"], true, None);
    assert_silence_then_onset(&samples);
}

#[test]
fn fof_onset_is_audible_after_live_clock_mode_switch() {
    // Server starts in transport mode (frozen — the harness never rolls
    // transport here) and a client switches it to frame-time at runtime,
    // proving `set_clock_mode` actually redirects the server's per-block
    // sample clock, not just the shared flag a reader sees.
    //
    // Deliberately a *forward* jump (transport's frame count, likely small
    // or 0, to frame-time's — jack_frame_time() counts from when the JACK
    // server itself started, so it's typically already far larger). The
    // reverse direction is a known gap: the timing wheel's clock is
    // monotonic-only (see `Wheel::advance` in src/queue.rs), so jumping to a
    // *smaller* reported clock leaves new deadlines looking already-past
    // until the new source's value grows back past where the wheel had
    // already reached — the same underlying issue as the still-open
    // "Handle transport jumps" item in dev_notes.md.
    let samples = run_fof_smoke_test(
        &["--clock-mode", "transport"],
        false,
        Some(rfofs::clock::RFOFS_CLOCK_JACK_FRAME_TIME),
    );
    assert_silence_then_onset(&samples);
}

/// Online-server counterpart to `continuous_fofs_produce_signal_throughout`
/// in `tests/offline_render.rs`'s sibling `tests/continuous_fofs.rs`: drips
/// ~0.2 s FOFs into a live `rfofs` server at 10-20/sec for about a minute of
/// real (wall-clock) recording via `jack_probe`, and checks the recorded
/// signal never goes silent in between.
///
/// Unlike `run_fof_smoke_test`'s single mid-recording onset, onsets here are
/// submitted progressively — each scheduled a fixed `lead_secs` ahead of the
/// server's own live `current_sample()` — rather than all at once, since the
/// time wheel's horizon (`n_slots * block_size` samples, see `src/queue.rs`)
/// is far shorter than the full minute.
#[test]
fn continuous_fofs_produce_signal_throughout_on_jack_output() {
    let _serial = serial_guard();

    let shm_name = format!("/rfofs_ctl_test_continuous_{}", std::process::id());
    let _shm_cleanup = ShmCleanup(CString::new(shm_name.clone()).unwrap());
    // SAFETY: serialized by `serial_guard()` above — no concurrent access to
    // the process environment from other tests in this binary.
    unsafe { std::env::set_var(SHM_NAME_ENV, &shm_name) };

    let (harness_client, _status) = jack::Client::new(
        "rfofs_test_harness_continuous",
        jack::ClientOptions::NO_START_SERVER,
    )
    .expect("failed to open JACK client — is a JACK server running?");
    let existing_ports =
        harness_client.ports(Some("^rfofs.*:out_.*$"), None, jack::PortFlags::IS_OUTPUT);

    let _server = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_rfofs"))
            .env(SHM_NAME_ENV, &shm_name)
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
    let sample_rate = client_shm.block().sample_rate();

    let push_duration_secs = 60.0f64;
    let fofs_per_second = 15.0f64; // within the requested 10-20/s range
    let interval = Duration::from_secs_f64(1.0 / fofs_per_second);
    // Schedule each onset this far ahead of "now" — needs enough slack to
    // absorb real scheduling jitter (bridging-thread poll latency, JACK
    // callback scheduling, slower debug builds) that a single generously
    // pre-scheduled onset (see `run_fof_smoke_test`'s 0.5 s) doesn't have to
    // worry about, since here every onset is timed close to its deadline.
    let lead_secs = 0.2f64;

    // ~0.2 s grains: short attack, exponential decay tuned so the envelope
    // has fallen to fade_level (-60 dB) by ~0.2 s in — same shaping as
    // `tests/continuous_fofs.rs`'s offline counterpart.
    let beta = 0.005f32;
    let fade_level = 0.001f32;
    let fade_dur = 0.01f32;
    let fof_duration_s = 0.2f32;
    let alpha = -fade_level.ln() / (fof_duration_s - beta);

    let wav_path = format!(
        "{}/jack_probe_continuous_{}.wav",
        env!("CARGO_TARGET_TMPDIR"),
        std::process::id()
    );
    // A bit longer than the push loop so the last grain's tail gets captured.
    let probe_duration_secs = push_duration_secs + 2.0;

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
    // the drip starts, so recording has already started by the time the
    // first onset fires.
    std::thread::sleep(Duration::from_millis(300));

    let freqs = [261.63f32, 293.66, 329.63, 392.00, 440.00]; // C4 D4 E4 G4 A4
    let azms = [-1.0f32, 0.0, 1.0];
    let n_fofs = (push_duration_secs * fofs_per_second) as u64;

    for i in 0..n_fofs {
        let start_sample =
            client_shm.block().current_sample() + (sample_rate as f64 * lead_secs) as u64;
        client_shm
            .block()
            .try_push_fof(FofParams {
                id: 0,
                start_sample,
                f: freqs[(i as usize) % freqs.len()],
                gliss: 0.0,
                phi: 0.0,
                amp: 0.5,
                alpha,
                beta,
                fade_level,
                fade_dur,
                azm: azms[(i as usize) % azms.len()],
                elev: 0.0,
                distance: 1.0,
            })
            .expect("fof request ring full");
        std::thread::sleep(interval);
    }

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

    let n_frames = samples.len() / n_channels;
    assert!(n_frames > 0, "recorded wav should contain samples");

    // Trim the very start (before the first grain's attack) and stop
    // checking well before the drip itself stopped (`push_duration_secs`
    // after roughly `start_frame`) rather than near the end of the
    // recording: `probe_duration_secs` deliberately runs ~2 s longer than
    // the push loop so the *last* grain's tail gets captured on tape, and
    // that trailing stretch is expected to fall silent once the drip ends —
    // it's not part of what this test is checking. Real-time scheduling
    // jitter earns a more generous margin here than the offline
    // counterpart's, since onset timing is only wall-clock-approximate.
    let margin_frames = (sample_rate as usize) / 2; // 0.5 s
    let start_frame = margin_frames.min(n_frames);
    let push_frames = (push_duration_secs * sample_rate as f64) as usize;
    let end_frame = (start_frame + push_frames)
        .saturating_sub(margin_frames)
        .min(n_frames)
        .max(start_frame);

    // Any 100 ms window in the steady-state region must contain signal —
    // shorter than the ~67 ms gap between consecutive grain onsets, so a
    // real dropout can't hide between windows.
    let window_frames = (sample_rate as usize) / 10;
    let threshold = 1e-3; // a live recording's noise floor is higher than the offline WAV's

    let mut frame = start_frame;
    while frame < end_frame {
        let window_end = (frame + window_frames).min(end_frame);
        let has_signal = (frame..window_end).any(|fr| {
            (0..n_channels).any(|ch| samples[fr * n_channels + ch].abs() > threshold)
        });
        assert!(
            has_signal,
            "silence detected in [{:.3}s, {:.3}s)",
            frame as f32 / sample_rate,
            window_end as f32 / sample_rate,
        );
        frame = window_end;
    }
}

#[test]
fn set_clock_mode_rejects_unknown_values() {
    let _serial = serial_guard();
    let shm_name = format!("/rfofs_ctl_test_reject_{}", std::process::id());
    let _shm_cleanup = ShmCleanup(CString::new(shm_name.clone()).unwrap());
    // SAFETY: serialized by `serial_guard()` above.
    unsafe { std::env::set_var(SHM_NAME_ENV, &shm_name) };

    let _server = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_rfofs"))
            .env(SHM_NAME_ENV, &shm_name)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn rfofs server"),
    );

    let client_shm = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match ClientShm::attach() {
                Ok(shm) => break shm,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => panic!("failed to attach to rfofs control plane: {e:?}"),
            }
        }
    };

    let before = client_shm.block().clock_mode();
    assert!(!client_shm.block().set_clock_mode(0xDEAD_BEEF));
    assert_eq!(client_shm.block().clock_mode(), before, "rejected write must not mutate stored mode");
}
