use rfofs::fof::FofParams;
use rfofs::queue::{kill_queue, time_wheel};
use rfofs::shm::ServerShm;
use rfofs::{OfflineRenderer, PanMode, RfofsEngine};

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = args.get(1) {
        run_offline(path);
    } else {
        run_jack();
    }
}

// ── Offline mode ─────────────────────────────────────────────────────────────

fn run_offline(path: &str) {
    let sample_rate = 48_000.0f32;
    let block_size = 512;
    let pan_mode = PanMode::parse("stereo").expect("invalid pan mode");

    let mut renderer = OfflineRenderer::open(path, sample_rate, pan_mode, block_size)
        .expect("failed to open output file");

    // Demo sequence — start_sample values are weakly monotonic.
    // Pentatonic phrase: C4 E4 G4 C5 G4, one note every 0.2 s at 48 kHz.
    let notes: &[(u64, f32, f32)] = &[
        (9_600,  261.63,  0.0),   // C4 at 0.2 s,  centre
        (19_200, 329.63, -0.4),   // E4 at 0.4 s,  left
        (28_800, 392.00,  0.4),   // G4 at 0.6 s,  right
        (38_400, 523.25,  0.0),   // C5 at 0.8 s,  centre
        (48_000, 392.00, -0.4),   // G4 at 1.0 s,  left
    ];

    for &(start_sample, f, azm) in notes {
        renderer.add_fof(FofParams {
            id:           0,
            start_sample,
            f,
            gliss:        0.0,
            phi:          0.0,
            amp:          0.5,
            alpha:        10.0,    // ~0.7 s decay (rad/s)
            beta:         0.01,    // 10 ms attack (seconds)
            fade_level:   0.001,
            fade_dur:     0.01,
            azm,
            elev:         0.0,
            distance:     1.0,
        });
    }

    renderer.close();
    println!("wrote {path}");
}

// ── JACK real-time mode ───────────────────────────────────────────────────────

fn run_jack() {
    // ── Configuration ─────────────────────────────────────────────────────
    let pan_mode = PanMode::parse("stereo").expect("invalid pan mode");
    let n_channels = pan_mode.channel_count();

    // ── Queues ────────────────────────────────────────────────────────────
    // D = 256 samples (typical block size), N = 256 slots -> horizon ~= 65.5k
    // samples (~1.4 s @48kHz), M = 64 simultaneous onsets per slot.
    let (mut wheel_tx, mut wheel_rx) = time_wheel(4096, 256, 256, 64);
    let (mut kill_tx, kill_rx) = kill_queue(256);

    // ── Control-plane shared memory ──────────────────────────────────────
    // Lets an external process (e.g. Racket via the rfofs-client cdylib)
    // submit FOFs/kills and read live stats. See rfofs::shm for the
    // cross-process ring buffer this wraps; wheel_rx's stats sink writes
    // directly into the shared segment, so external readers see them live
    // with no separate sync step.
    let shm = ServerShm::create().expect("failed to create control-plane shm segment");
    wheel_rx.attach_stats(&shm.block().stats);

    // ── JACK client ───────────────────────────────────────────────────────
    let (client, _status) =
        jack::Client::new("rfofs", jack::ClientOptions::NO_START_SERVER)
            .expect("failed to open JACK client");

    let sample_rate = client.sample_rate() as f32;
    let max_block_size = client.buffer_size() as usize;

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
    let process = jack::ClosureProcessHandler::new(
        move |_client, ps: &jack::ProcessScope| -> jack::Control {
            let block_size = ps.n_frames() as usize;

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

            jack::Control::Continue
        },
    );

    // ── Activate ──────────────────────────────────────────────────────────
    let _active_client = client.activate_async((), process)
        .expect("failed to activate JACK client");

    // ── Demo: enqueue a single test FOF from the main thread ─────────────
    let params = FofParams {
        id:           0,          // fire-and-forget
        start_sample: 4410,       // start 0.1 s from engine start (at 44100 Hz)
        f:            440.0,
        gliss:        1.0,        // glide up one octave/sec
        phi:          0.0,
        amp:          0.5,
        alpha:        10.0,        // ~0.7 s decay (rad/s)
        beta:         0.01,       // 10 ms attack (seconds)
        fade_level:   0.001,
        fade_dur:     0.01,       // 10 ms fade-out
        azm:          0.0,
        elev:         0.0,
        distance:     1.0,
    };

    wheel_tx.push(params).expect("queue full");

    // ── Control-plane bridging thread ────────────────────────────────────
    // Not real-time: forwards shared-memory requests from external clients
    // into the same TimeWheelProducer/KillQueueProducer the demo push above
    // uses. A short sleep when both rings are empty avoids busy-spinning a
    // whole core — FOF onsets are scheduled via start_sample ahead of time,
    // so sub-millisecond latency here is a non-issue.
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

    println!("rfofs running — press Enter to quit");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
}
