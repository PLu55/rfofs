//! Renders a single FOF (same params as `profile_compute::standard_fof`) to
//! completion and dumps `time_seconds amplitude` pairs to a data file for
//! plotting with gnuplot.
//!
//! Usage: cargo run --release --example plot_single_fof [output.dat]

use std::io::Write;

use rfofs::{
    fof::FofParams,
    pan::PanMode,
    queue::{kill_queue, time_wheel},
    RfofsEngine,
};

const SAMPLE_RATE: f32 = 48000.0;
const BLOCK_SIZE: usize = 256;

fn standard_fof() -> FofParams {
    FofParams {
        id: 0,
        start_sample: 0,
        f: 1000.0,
        gliss: 0.0,
        phi: 0.0,
        amp: 1.0,
        alpha: 13.5,
        beta: 0.1,
        fade_level: 0.001,
        fade_dur: 0.005,
        azm: 0.0,
        elev: 0.0,
        distance: 0.0,
    }
}

fn main() {
    let out_path = std::env::args().nth(1).unwrap_or_else(|| "fof.dat".to_string());

    let (mut wheel_tx, wheel_rx) = time_wheel(1, 4, BLOCK_SIZE as u64, 1);
    let (_kill_tx, kill_rx) = kill_queue(64);
    wheel_tx.push(standard_fof()).expect("queue full");
    drop(wheel_tx);

    let mut engine = RfofsEngine::new(SAMPLE_RATE, PanMode::Mono, 1, BLOCK_SIZE, vec![wheel_rx], kill_rx);

    let mut ch0 = vec![0.0f32; BLOCK_SIZE];
    let mut ch1 = vec![0.0f32; BLOCK_SIZE];
    let mut file = std::fs::File::create(&out_path).expect("create output file");
    let mut sample_idx: u64 = 0;

    loop {
        ch0.fill(0.0);
        ch1.fill(0.0);
        engine.process_block(&mut [&mut ch0, &mut ch1], BLOCK_SIZE);
        for &s in &ch0 {
            let t = sample_idx as f64 / SAMPLE_RATE as f64;
            writeln!(file, "{t:.8} {s:.8}").unwrap();
            sample_idx += 1;
        }
        if engine.active_count() == 0 {
            break;
        }
    }

    eprintln!("wrote {sample_idx} samples to {out_path}");
}
