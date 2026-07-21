use clap::Parser;
use rfofs::clock::ClockMode;
use rfofs::queue::{kill_queue, time_wheel};
use rfofs::shm::ServerShm;
use rfofs::{PanMode, RfofsEngine};

/// Upper bound on the JACK-reported block size this process will accept.
/// `RfofsEngine`'s scratch buffers are preallocated to this size (see
/// `rfofs::engine`); JACK reporting a larger block at startup would force a
/// resize on the audio thread the first time it's hit, violating the
/// allocation-free `process_block` invariant, so we refuse to start instead.
const MAX_BLOCK_SIZE: usize = 1024;

/// `rfofs`'s online server: a JACK client whose FOF onsets and kills are
/// driven entirely by external processes (e.g. Racket via the
/// `rfofs-client` cdylib) over the shared-memory control plane in
/// `rfofs::shm`. See `rfofs-client/examples/shm_client_smoke.rs` for a
/// worked example of connecting and issuing calls.
///
/// (Offline WAV rendering via `OfflineRenderer` no longer has a CLI mode
/// here — see `tests/offline_render.rs`, which exercises it directly.)
#[derive(Parser)]
#[command(name = "rfofs", version, about = "Real-time FOF granular synthesizer (JACK server)")]
struct Cli {
    /// Initial JACK time source driving the engine's sample clock (see
    /// `rfofs::clock`): frame-time|transport|1|2. A connected client can
    /// switch it live afterwards via `rfofs-client`'s `rfofs_set_clock_mode`
    /// — the process callback re-reads the shared control block's clock
    /// mode every block rather than caching this startup value.
    #[arg(short = 'c', long = "clock-mode", default_value = "frame-time", value_parser = parse_clock_mode)]
    clock_mode: ClockMode,

    /// Panning mode (see `rfofs::pan`), fixed for the process's lifetime —
    /// output ports are registered from its channel count before the JACK
    /// client activates. One of `mono`, `stereo`, or
    /// `ambiO<n>D<2|3>[R<0|1>]` (e.g. `ambiO1D3`, `ambiO2D3R1`) — ambisonics
    /// of order `n`, `2`- or 3-dimensional, with an optional reverb bus
    /// (`R0` = mono, `R1` = first-order).
    #[arg(short = 'm', long = "mode", default_value = "stereo", value_parser = parse_pan_mode)]
    mode: PanMode,

    /// Capacity of the SPSC ring buffer carrying incoming FOF onsets from
    /// the control-plane bridging thread into the time wheel (see
    /// `rfofs::queue::time_wheel`'s `ingress_capacity`).
    #[arg(short = 'i', long = "ingress-capacity", default_value_t = 4096)]
    ingress_capacity: usize,

    /// Number of slots (`N`) in the onset time wheel. Combined with the
    /// slot duration (the JACK server's block size), this sets the wheel's
    /// scheduling horizon (`n_slots * block_size` samples).
    #[arg(short = 'n', long = "n-slots", default_value_t = 256)]
    n_slots: usize,

    /// Maximum simultaneous onsets per wheel slot (`M`) — see
    /// `rfofs::queue::Wheel`.
    #[arg(short = 'k', long = "slot-capacity", default_value_t = 64)]
    slot_capacity: usize,
}

fn parse_clock_mode(s: &str) -> Result<ClockMode, String> {
    ClockMode::parse(s).ok_or_else(|| format!("invalid clock mode: {s}"))
}

/// Parses the CLI's `-m`/`--mode` value: `mono`, `stereo`, or
/// `ambiO<n>D<2|3>[R<0|1>]` (e.g. `ambiO1D3`, `ambiO2D3R1`). This is a
/// stricter, unspaced sibling of `PanMode::parse`'s "amb O2 D3 R1" notation,
/// purpose-built for shell arguments.
fn parse_pan_mode(s: &str) -> Result<PanMode, String> {
    if s.eq_ignore_ascii_case("mono") {
        return Ok(PanMode::Mono);
    }
    if s.eq_ignore_ascii_case("stereo") {
        return Ok(PanMode::Stereo);
    }

    let invalid = || format!("invalid mode: {s}");

    let rest = s.strip_prefix("ambi").ok_or_else(invalid)?;
    let rest = rest.strip_prefix('O').ok_or_else(invalid)?;
    let d_pos = rest.find('D').ok_or_else(invalid)?;
    let order: u8 = rest[..d_pos].parse().map_err(|_| invalid())?;

    let after_d = &rest[d_pos + 1..];
    let (dims_str, reverb_str) = match after_d.find('R') {
        Some(r_pos) => (&after_d[..r_pos], Some(&after_d[r_pos + 1..])),
        None => (after_d, None),
    };
    let dims: u8 = dims_str.parse().map_err(|_| invalid())?;
    if dims != 2 && dims != 3 {
        return Err(invalid());
    }

    let reverb = match reverb_str {
        None => None,
        Some(r) => {
            let k: u8 = r.parse().map_err(|_| invalid())?;
            if k != 0 && k != 1 {
                return Err(invalid());
            }
            Some(k)
        }
    };

    Ok(PanMode::Ambisonic { order, dims, reverb })
}

fn main() {
    let cli = Cli::parse();
    env_logger::init();

    // ── Configuration ─────────────────────────────────────────────────────
    let pan_mode = cli.mode;
    let n_channels = pan_mode.channel_count();
    let clock_mode = cli.clock_mode;

    // ── JACK client ───────────────────────────────────────────────────────
    let (client, _status) =
        jack::Client::new("rfofs", jack::ClientOptions::NO_START_SERVER)
            .expect("failed to open JACK client");

    let sample_rate = client.sample_rate() as f32;
    let block_size = client.buffer_size() as usize;
    if block_size > MAX_BLOCK_SIZE {
        eprintln!(
            "JACK block size {block_size} exceeds the maximum this server supports ({MAX_BLOCK_SIZE}); \
             reconfigure JACK with a smaller buffer size and restart."
        );
        std::process::exit(1);
    }

    // ── Queues ────────────────────────────────────────────────────────────
    // D = JACK's current block size (the wheel fires in lockstep with the
    // process callback, so this is the natural slot granularity); N
    // (n_slots) and M (slot_capacity) are CLI-configurable, defaulting to a
    // horizon of n_slots * block_size samples with up to 64 simultaneous
    // onsets per slot.
    let (mut wheel_tx, mut wheel_rx) =
        time_wheel(cli.ingress_capacity, cli.n_slots, block_size as u64, cli.slot_capacity);
    let (mut kill_tx, kill_rx) = kill_queue(256);

    // ── Control-plane shared memory ──────────────────────────────────────
    // Lets an external process (e.g. Racket via the rfofs-client cdylib)
    // submit FOFs/kills and read live stats. See rfofs::shm for the
    // cross-process ring buffer this wraps; wheel_rx's stats sink writes
    // directly into the shared segment, so external readers see them live
    // with no separate sync step. Published alongside the actual sample
    // rate/buffer size JACK just handed us, so clients don't have to assume
    // fixed values.
    let shm = ServerShm::create(sample_rate, block_size as u32, clock_mode.as_u32())
        .expect("failed to create control-plane shm segment");
    let shm_block = shm.block();
    wheel_rx.attach_stats(&shm_block.stats);

    // Register output ports.
    let mut out_ports: Vec<jack::Port<jack::AudioOut>> = (0..n_channels)
        .map(|i| {
            client
                .register_port(&format!("out_{}", i), jack::AudioOut::default())
                .expect("failed to register output port")
        })
        .collect();

    // ── Engine ────────────────────────────────────────────────────────────
    let mut engine = RfofsEngine::new(
        sample_rate,
        pan_mode,
        4096,            // initial FOF pool capacity
        MAX_BLOCK_SIZE,
        vec![wheel_rx],
        kill_rx,
    );

    // ── Process callback ──────────────────────────────────────────────────
    let process = jack::contrib::ClosureProcessHandler::new(
        move |client: &jack::Client, ps: &jack::ProcessScope| -> jack::Control {
            let block_size = ps.n_frames() as usize;

            // Resync the engine's sample clock to the currently selected
            // JACK time source before processing this block — see
            // `rfofs::clock`. Re-read from the shared control block every
            // block (instead of the CLI-supplied startup value) so a
            // connected client's `rfofs_set_clock_mode` call takes effect
            // live. `JackTransport` falls back to the engine's own running
            // clock if the query fails (e.g. client shutting down), rather
            // than stalling the callback; an unrecognized stored value
            // (shouldn't happen — `set_clock_mode` validates writes) falls
            // back to the CLI-selected startup mode.
            let active_clock_mode =
                ClockMode::from_u32(shm_block.clock_mode()).unwrap_or(clock_mode);
            let block_start = match active_clock_mode {
                ClockMode::JackFrameTime => client.frame_time() as u64,
                ClockMode::JackTransport => client
                    .transport()
                    .query()
                    .map(|tp| tp.pos.frame() as u64)
                    .unwrap_or_else(|_| engine.sample_clock()),
            };
            engine.set_sample_clock(block_start);

            // Get mutable slices for each output channel.
            let mut slices: Vec<&mut [f32]> = out_ports
                .iter_mut()
                .map(|p| p.as_mut_slice(ps))
                .collect();

            // Zero output buffers before accumulation.
            for s in slices.iter_mut() {
                s.iter_mut().for_each(|x| *x = 0.0);
            }

            engine.process_block(&mut slices, block_size);
            shm_block.set_current_sample(engine.sample_clock());

            jack::Control::Continue
        },
    );

    // ── Activate ──────────────────────────────────────────────────────────
    let _active_client = client.activate_async((), process)
        .expect("failed to activate JACK client");

    // ── Control-plane bridging thread ────────────────────────────────────
    // Not real-time: forwards shared-memory requests from external clients
    // into the same TimeWheelProducer/KillQueueProducer the engine above was
    // constructed with. A short sleep when both rings are empty avoids
    // busy-spinning a whole core — FOF onsets are scheduled via start_sample
    // ahead of time, so sub-millisecond latency here is a non-issue.
    std::thread::spawn(move || {
        let block = shm.block();
        loop {
            let mut did_work = false;
            if let Some(p) = block.try_pop_fof() {
                let _ = wheel_tx.push(p);
                did_work = true;
            }
            if let Some(k) = block.try_pop_kill() {
                let _ = kill_tx.push(k);
                did_work = true;
            }
            if !did_work {
                std::thread::sleep(std::time::Duration::from_micros(500));
            }
        }
    });

    println!("rfofs server running — connect with rfofs-client, press Enter to quit");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
}
