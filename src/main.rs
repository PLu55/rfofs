use rfofs::clock::ClockMode;
use rfofs::queue::{kill_queue, time_wheel};
use rfofs::shm::ServerShm;
use rfofs::{PanMode, RfofsEngine};

/// `rfofs`'s online server: a JACK client whose FOF onsets and kills are
/// driven entirely by external processes (e.g. Racket via the
/// `rfofs-client` cdylib) over the shared-memory control plane in
/// `rfofs::shm`. See `rfofs-client/examples/shm_client_smoke.rs` for a
/// worked example of connecting and issuing calls.
///
/// (Offline WAV rendering via `OfflineRenderer` no longer has a CLI mode
/// here — see `tests/offline_render.rs`, which exercises it directly.)
///
/// Pass `--clock-mode <frame-time|transport|1|2>` to pick the *initial*
/// JACK time source driving the engine's sample clock (see `rfofs::clock`);
/// defaults to `frame-time`. A connected client can switch it live via
/// `rfofs-client`'s `rfofs_set_clock_mode` — the process callback re-reads
/// the shared control block's clock mode every block rather than caching
/// this startup value.
fn parse_clock_mode_arg() -> ClockMode {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--clock-mode=") {
            return ClockMode::parse(value).expect("invalid --clock-mode value");
        }
        if arg == "--clock-mode" {
            let value = args.next().expect("--clock-mode requires a value");
            return ClockMode::parse(&value).expect("invalid --clock-mode value");
        }
    }
    ClockMode::default()
}

fn main() {
    env_logger::init();

    // ── Configuration ─────────────────────────────────────────────────────
    let pan_mode = PanMode::parse("stereo").expect("invalid pan mode");
    let n_channels = pan_mode.channel_count();
    let clock_mode = parse_clock_mode_arg();

    // ── Queues ────────────────────────────────────────────────────────────
    // D = 256 samples (typical block size), N = 256 slots -> horizon ~= 65.5k
    // samples (~1.4 s @48kHz), M = 64 simultaneous onsets per slot.
    let (mut wheel_tx, mut wheel_rx) = time_wheel(4096, 256, 256, 64);
    let (mut kill_tx, kill_rx) = kill_queue(256);

    // ── JACK client ───────────────────────────────────────────────────────
    let (client, _status) =
        jack::Client::new("rfofs", jack::ClientOptions::NO_START_SERVER)
            .expect("failed to open JACK client");

    let sample_rate = client.sample_rate() as f32;
    let max_block_size = client.buffer_size() as usize;

    // ── Control-plane shared memory ──────────────────────────────────────
    // Lets an external process (e.g. Racket via the rfofs-client cdylib)
    // submit FOFs/kills and read live stats. See rfofs::shm for the
    // cross-process ring buffer this wraps; wheel_rx's stats sink writes
    // directly into the shared segment, so external readers see them live
    // with no separate sync step. Published alongside the actual sample
    // rate/buffer size JACK just handed us, so clients don't have to assume
    // fixed values.
    let shm = ServerShm::create(sample_rate, max_block_size as u32, clock_mode.as_u32())
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
        max_block_size,
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
