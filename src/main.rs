use rfofs::{PanMode, RfofsEngine};
use rfofs::fof::FofParams;
use rfofs::queue::{kill_queue, time_wheel};

fn main() {
    env_logger::init();

    // ── Configuration ─────────────────────────────────────────────────────
    let pan_mode = PanMode::parse("stereo").expect("invalid pan mode");
    let n_channels = pan_mode.channel_count();

    // ── Queues ────────────────────────────────────────────────────────────
    let (mut wheel_tx, wheel_rx) = time_wheel(4096);
    let (mut kill_tx, kill_rx) = kill_queue(256);

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
        alpha:        10.0,       // moderate decay
        beta:         441.0,      // 10 ms attack at 44100 Hz
        fade_level:   0.001,
        fade_dur:     441,        // 10 ms fade-out
        azm:          0.0,
        elev:         0.0,
        distance:     1.0,
    };

    wheel_tx.push(params).expect("queue full");

    println!("rfofs running — press Enter to quit");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
}
