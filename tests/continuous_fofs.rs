//! Integration test that streams a steady drip of short FOFs into the
//! offline renderer — 10-20 new grains per second, each ~0.2 s long — for
//! about a minute of audio, and checks that the rendered signal never goes
//! silent in between.

use rfofs::fof::FofParams;
use rfofs::{OfflineRenderer, PanMode};
use sndfile::{OpenOptions, ReadOptions, SndFileIO};

#[test]
fn continuous_fofs_produce_signal_throughout() {
    let path = format!("{}/continuous_fofs.wav", env!("CARGO_TARGET_TMPDIR"));

    let sample_rate = 48_000.0f32;
    let block_size = 512;
    let pan_mode = PanMode::parse("stereo").expect("invalid pan mode");

    let duration_s = 60.0; // ~1 minute of rendered audio
    let fofs_per_second = 15.0; // within the requested 10-20/s range
    let interval_samples = (sample_rate / fofs_per_second) as u64;
    let n_fofs = (duration_s * fofs_per_second) as u64;

    // ~0.2 s grains: short attack, exponential decay tuned so the envelope
    // has fallen to fade_level (-60 dB) by ~0.2 s in.
    let beta = 0.005; // 5 ms attack
    let fade_level = 0.001f32;
    let fade_dur = 0.01;
    let fof_duration_s = 0.2;
    let alpha = -fade_level.ln() / (fof_duration_s - beta);

    let mut renderer = OfflineRenderer::open(&path, sample_rate, pan_mode, block_size)
        .expect("failed to open output file");

    let freqs = [261.63, 293.66, 329.63, 392.00, 440.00]; // C4 D4 E4 G4 A4
    let azms = [-1.0, 0.0, 1.0];

    for i in 0..n_fofs {
        renderer.add_fof(FofParams {
            id: 0,
            start_sample: i * interval_samples,
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
        });
    }

    renderer.close();

    let mut snd = OpenOptions::ReadOnly(ReadOptions::Auto)
        .from_path(&path)
        .expect("failed to reopen rendered wav");
    assert_eq!(snd.get_samplerate(), sample_rate as usize);
    let n_channels = snd.get_channels();
    assert_eq!(n_channels, 2);

    let samples: Vec<f32> = SndFileIO::<f32>::read_all_to_vec(&mut snd)
        .expect("failed to read back rendered samples");
    let n_frames = samples.len() / n_channels;
    assert!(n_frames > 0, "rendered wav should contain samples");

    // Skip the very start (before the first grain's attack ramps up) and
    // the tail (after the last grain has fully decayed) — the point of
    // this test is that FOFs overlap enough to keep the *steady-state*
    // signal continuous, not that there's sound before the first onset or
    // after the last one dies out.
    let warmup_frames = (sample_rate * 0.3) as usize;
    let start_frame = warmup_frames.min(n_frames);
    let end_frame = n_frames.saturating_sub(warmup_frames).max(start_frame);

    // Any 50 ms window in the steady-state region must contain signal —
    // shorter than the ~67 ms gap between consecutive grain onsets, so a
    // real dropout can't hide between windows.
    let window_frames = (sample_rate * 0.05) as usize;
    let threshold = 1e-4;

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
