//! Small standalone diagnostic tool: connects to a range of existing JACK
//! output ports, measures their signal level (RMS / dBFS), and optionally
//! records what it hears to a multichannel WAV file via `libsndfile`.
//!
//! This is a passive probe — it only *reads* whatever is already flowing on
//! the JACK graph (e.g. `rfofs`'s own `out_0`/`out_1` ports, or any other
//! client's outputs) and is otherwise independent of the `rfofs` engine.
//!
//! Usage:
//!   jack_probe [--pattern <regex>] [--count <N>] [--start <N>] [--output <path.wav>] [--duration <secs>]
//!
//! `--pattern` (default `rfofs:out_.*`) is matched against existing JACK
//! output port names using JACK's own POSIX regex port matching (see
//! `jack::Client::ports`). `--start`/`--count` (default 1) select a slice of
//! the matches (in JACK's own registration-order listing) to actually
//! probe — e.g. `--pattern 'rfofs:out_.*' --start 0 --count 2` probes the
//! first two `rfofs:out_*` ports.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rtrb::RingBuffer;
use sndfile::{Endian, MajorFormat, OpenOptions, SndFile, SndFileIO, SubtypeFormat, WriteOptions};

#[derive(Parser)]
#[command(
    name = "jack_probe",
    version,
    about = "Connect to existing JACK output ports, measure RMS/dBFS, and optionally record to WAV"
)]
struct Args {
    /// POSIX regex matched against existing JACK output port names"
    #[arg(short = 'p', long, default_value = "rfofs:out_.*")]
    pattern: String,

    /// Index of the first match to probe
    #[arg(short = 's', long, default_value_t = 0)]
    start: usize,

    /// Number of matched ports to probe, starting at --start
    #[arg(short = 'n', long, default_value_t = 1, value_parser = parse_nonzero_count)]
    count: usize,

    /// Write the probed signal to a multichannel WAV file
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// Seconds to run before stopping (default: run until Enter is pressed)
    #[arg(short = 'd', long)]
    duration: Option<f64>,
}

fn parse_nonzero_count(s: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|_| format!("invalid count: {s}"))?;
    if n == 0 {
        return Err("--count must be > 0".to_string());
    }
    Ok(n)
}

/// Accumulated over the run by the writer thread, off the JACK real-time
/// thread — see the module doc comment for the producer/consumer split.
struct WriterResult {
    sum_sq: Vec<f64>,
    frame_count: u64,
}

fn main() {
    env_logger::init();
    let args = Args::parse();

    let (client, _status) = jack::Client::new("rfofs_probe", jack::ClientOptions::NO_START_SERVER)
        .expect("failed to open JACK client");
    let sample_rate = client.sample_rate() as f64;

    let matches = client.ports(Some(&args.pattern), None, jack::PortFlags::IS_OUTPUT);
    if args.start + args.count > matches.len() {
        eprintln!(
            "requested ports [{}, {}) but only {} JACK output port(s) matched pattern {:?}:",
            args.start,
            args.start + args.count,
            matches.len(),
            args.pattern
        );
        for m in &matches {
            eprintln!("  {m}");
        }
        std::process::exit(1);
    }
    let targets = matches[args.start..args.start + args.count].to_vec();
    let n_channels = targets.len();

    println!("probing {n_channels} port(s):");
    for t in &targets {
        println!("  {t}");
    }

    let in_ports: Vec<jack::Port<jack::AudioIn>> = (0..n_channels)
        .map(|i| {
            client
                .register_port(&format!("probe_{i}"), jack::AudioIn::default())
                .expect("failed to register input port")
        })
        .collect();
    let in_port_names: Vec<String> = in_ports.iter().map(|p| p.name().unwrap()).collect();

    // ── Cross-thread handoff ────────────────────────────────────────────
    // The audio callback only pushes raw interleaved samples into a
    // lock-free SPSC ring buffer (`rtrb`, same crate the engine's own
    // queues use) — all RMS accumulation and file I/O happens on the
    // writer thread below, off the real-time path. Capacity is sized for a
    // couple of seconds of headroom against scheduling jitter; the writer
    // drains continuously, so this bounds jitter tolerance, not total run
    // length.
    let ring_capacity = (n_channels * sample_rate as usize * 2).max(4096);
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(ring_capacity);
    let dropped_frames = Arc::new(AtomicU64::new(0));
    let dropped_frames_rt = Arc::clone(&dropped_frames);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_writer = Arc::clone(&shutdown);

    let output_path = args.output.clone();
    let writer = std::thread::spawn(move || -> WriterResult {
        let mut snd: Option<SndFile> = output_path.as_ref().map(|path| {
            OpenOptions::WriteOnly(WriteOptions::new(
                MajorFormat::WAV,
                SubtypeFormat::FLOAT,
                Endian::File,
                sample_rate as usize,
                n_channels,
            ))
            .from_path(path)
            .expect("failed to open output WAV file")
        });

        let mut sum_sq = vec![0.0f64; n_channels];
        let mut frame_count = 0u64;
        let mut total_samples = 0u64;
        // Flushed to the WAV file in chunks rather than per-sample, same
        // as OfflineRenderer's per-block writes in src/offline.rs.
        let mut chunk = vec![0.0f32; n_channels * 4096];
        let mut chunk_len = 0usize;

        loop {
            match consumer.pop() {
                Ok(sample) => {
                    let ch = (total_samples % n_channels as u64) as usize;
                    sum_sq[ch] += (sample as f64) * (sample as f64);
                    total_samples += 1;
                    if ch == n_channels - 1 {
                        frame_count += 1;
                    }
                    chunk[chunk_len] = sample;
                    chunk_len += 1;
                    if chunk_len == chunk.len() {
                        if let Some(snd) = snd.as_mut() {
                            <SndFile as SndFileIO<f32>>::write_from_slice(snd, &chunk)
                                .expect("sndfile write failed");
                        }
                        chunk_len = 0;
                    }
                }
                Err(_) => {
                    if shutdown_writer.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_micros(500));
                }
            }
        }
        if chunk_len > 0
            && let Some(snd) = snd.as_mut()
        {
            <SndFile as SndFileIO<f32>>::write_from_slice(snd, &chunk[..chunk_len])
                .expect("sndfile write failed");
        }
        WriterResult { sum_sq, frame_count }
    });

    let process = jack::contrib::ClosureProcessHandler::new(
        move |_: &jack::Client, ps: &jack::ProcessScope| -> jack::Control {
            let slices: Vec<&[f32]> = in_ports.iter().map(|p| p.as_slice(ps)).collect();
            let n_frames = ps.n_frames() as usize;
            for i in 0..n_frames {
                // Check room for the whole frame up front — a partial push
                // (some channels but not others) would desync the writer's
                // channel-tracking for every sample after it.
                if producer.slots() < n_channels {
                    dropped_frames_rt.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                for s in &slices {
                    let _ = producer.push(s[i]);
                }
            }
            jack::Control::Continue
        },
    );

    let active_client = client
        .activate_async((), process)
        .expect("failed to activate JACK client");
    for (target, probe_port) in targets.iter().zip(in_port_names.iter()) {
        active_client
            .as_client()
            .connect_ports_by_name(target, probe_port)
            .unwrap_or_else(|e| panic!("failed to connect {target} -> {probe_port}: {e}"));
    }

    match args.duration {
        Some(secs) => {
            println!("recording for {secs:.2}s...");
            std::thread::sleep(Duration::from_secs_f64(secs));
        }
        None => {
            println!("probing — press Enter to stop");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).ok();
        }
    }

    // Stop the process callback before draining, so the writer sees a
    // final, unmoving ring buffer rather than racing new pushes.
    active_client.deactivate().expect("failed to deactivate JACK client");
    shutdown.store(true, Ordering::Release);
    let result = writer.join().expect("writer thread panicked");

    println!();
    for (i, target) in targets.iter().enumerate() {
        let mean_sq = if result.frame_count > 0 {
            result.sum_sq[i] / result.frame_count as f64
        } else {
            0.0
        };
        let rms = mean_sq.sqrt();
        let dbfs = if rms > 0.0 { 20.0 * rms.log10() } else { f64::NEG_INFINITY };
        println!("{target}: RMS = {rms:.6}  ({dbfs:.1} dBFS)");
    }

    let dropped = dropped_frames.load(Ordering::Relaxed);
    if dropped > 0 {
        eprintln!("warning: {dropped} frame(s) dropped (writer thread fell behind)");
    }
    if let Some(path) = &args.output {
        println!("wrote {} frame(s) to {}", result.frame_count, path.display());
    }
}
