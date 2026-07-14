//! Integration test for the offline WAV-rendering path (`OfflineRenderer`).
//!
//! Formerly `src/main.rs`'s offline CLI mode (`cargo run --release --
//! output.wav`) — that demo phrase now lives here as an automated test
//! instead, since it needs no JACK server or other runtime environment.
//! `main.rs` itself is now the online server (see `rfofs::shm`).

use rfofs::fof::FofParams;
use rfofs::{OfflineRenderer, PanMode};
use sndfile::{OpenOptions, ReadOptions, SndFileIO};

#[test]
fn offline_render_produces_nonsilent_wav() {
    let path = format!("{}/offline_render_demo.wav", env!("CARGO_TARGET_TMPDIR"));

    let sample_rate = 48_000.0f32;
    let block_size = 512;
    let pan_mode = PanMode::parse("stereo").expect("invalid pan mode");

    let mut renderer = OfflineRenderer::open(&path, sample_rate, pan_mode, block_size)
        .expect("failed to open output file");

    // Pentatonic phrase: C4 E4 G4 C5 G4, one note every 0.2 s at 48 kHz.
    let notes: &[(u64, f32, f32)] = &[
        (9_600, 261.63, 0.0),   // C4 at 0.2 s,  centre
        (19_200, 329.63, -1.0), // E4 at 0.4 s,  left
        (28_800, 392.00, 1.0),  // G4 at 0.6 s,  right
        (38_400, 523.25, 0.0),  // C5 at 0.8 s,  centre
        (48_000, 392.00, -1.0), // G4 at 1.0 s,  left
    ];

    for &(start_sample, f, azm) in notes {
        renderer.add_fof(FofParams {
            id: 0,
            start_sample,
            f,
            gliss: 0.0,
            phi: 0.0,
            amp: 0.5,
            alpha: 10.0,      // ~0.7 s decay (rad/s)
            beta: 0.01,       // 10 ms attack (seconds)
            fade_level: 0.001,
            fade_dur: 0.01,
            azm,
            elev: 0.0,
            distance: 1.0,
        });
    }

    renderer.close();

    let mut snd = OpenOptions::ReadOnly(ReadOptions::Auto)
        .from_path(&path)
        .expect("failed to reopen rendered wav");
    assert_eq!(snd.get_samplerate(), sample_rate as usize);
    assert_eq!(snd.get_channels(), 2);

    let samples: Vec<f32> = SndFileIO::<f32>::read_all_to_vec(&mut snd)
        .expect("failed to read back rendered samples");
    assert!(!samples.is_empty(), "rendered wav should contain samples");
    assert!(
        samples.iter().any(|&s| s.abs() > 1e-4),
        "rendered audio should not be silent"
    );
}
