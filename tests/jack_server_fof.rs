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
/// again on return/panic via `StopTransportOnDrop`.
fn run_fof_smoke_test(extra_args: &[&str], roll_transport: bool) -> Vec<f32> {
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
    let samples = run_fof_smoke_test(&[], false);
    assert_silence_then_onset(&samples);
}

#[test]
fn fof_onset_is_audible_with_transport_clock_mode() {
    let samples = run_fof_smoke_test(&["--clock-mode", "transport"], true);
    assert_silence_then_onset(&samples);
}
