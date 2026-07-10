use std::path::Path;

use sndfile::{Endian, MajorFormat, OpenOptions, SndFile, SndFileIO, SubtypeFormat, WriteOptions};

use crate::fof::FofParams;
use crate::queue::{kill_queue, time_wheel, TimeWheelProducer};
use crate::{PanMode, RfofsEngine};

// ─────────────────────────────────────────────────────────────────────────────

/// Offline FOF synthesiser that writes directly to a sound file.
///
/// FOFs are submitted via [`add_fof`](OfflineRenderer::add_fof). Blocks are
/// rendered and written to the file on demand, driven by each FOF's
/// `start_sample`. Call [`close`](OfflineRenderer::close) when all FOFs have
/// been submitted to flush remaining audio and finalise the file.
pub struct OfflineRenderer {
    engine: RfofsEngine,
    wheel_tx: TimeWheelProducer,
    sample_rate: f32,
    block_size: usize,
    n_channels: usize,
    /// Per-channel scratch buffers (length = block_size each).
    channel_bufs: Vec<Vec<f32>>,
    /// Interleaved scratch written to the file each block.
    interleaved: Vec<f32>,
    snd: SndFile,
    /// Largest `start_sample` seen so far — enforces weak monotonicity.
    last_start_sample: u64,
}

impl OfflineRenderer {
    /// Open `path` for writing and prepare the renderer.
    ///
    /// The output is a WAV file with 32-bit float samples.
    pub fn open(
        path: impl AsRef<Path>,
        sample_rate: f32,
        pan_mode: PanMode,
        block_size: usize,
    ) -> Result<Self, sndfile::SndFileError> {
        let n_channels = pan_mode.channel_count();
        let snd = OpenOptions::WriteOnly(WriteOptions::new(
            MajorFormat::WAV,
            SubtypeFormat::FLOAT,
            Endian::File,
            sample_rate as usize,
            n_channels,
        ))
        .from_path(path)?;

        // D = 256 samples (typical block size), N = 256 slots -> horizon ~=
        // 65.5k samples (~1.4 s @48kHz), M = 64 simultaneous onsets per slot.
        let (wheel_tx, wheel_rx) = time_wheel(4096, 256, 256, 64);
        let (_kill_tx, kill_rx) = kill_queue(256);

        let engine = RfofsEngine::new(
            sample_rate,
            pan_mode,
            4096,
            block_size,
            vec![wheel_rx],
            kill_rx,
        );

        Ok(OfflineRenderer {
            engine,
            wheel_tx,
            sample_rate,
            block_size,
            n_channels,
            channel_bufs: vec![vec![0.0f32; block_size]; n_channels],
            interleaved: vec![0.0f32; block_size * n_channels],
            snd,
            last_start_sample: 0,
        })
    }

    /// Submit a FOF for synthesis.
    ///
    /// `params.start_sample` must be ≥ the `start_sample` of the previously
    /// submitted FOF (weakly monotonic). Blocks are rendered and written to
    /// the file as the engine clock advances to stay just behind
    /// `params.start_sample`: a block is processed whenever
    /// `start_sample > engine_clock + block_size`.
    pub fn add_fof(&mut self, params: FofParams) {
        assert!(
            params.start_sample >= self.last_start_sample,
            "start_sample {} < previous {} — FOF start times must be weakly monotonic",
            params.start_sample,
            self.last_start_sample,
        );
        self.wheel_tx.push(params).expect("time wheel full");
        let block_size = self.block_size as u64;
        while params.start_sample > self.engine.sample_clock() + block_size {
            self.process_one_block();
        }
        self.last_start_sample = params.start_sample;
    }

    /// Flush all remaining audio and close the output file.
    ///
    /// Renders blocks until every scheduled and active FOF has fully decayed.
    /// A safety limit of 30 seconds prevents infinite loops if a FOF never
    /// reaches the `Dead` phase (which should not happen with valid parameters).
    pub fn close(mut self) {
        // Advance until the upcoming block window covers last_start_sample,
        // so drain_block_safe will pick up the last queued FOF.
        let bs = self.block_size as u64;
        while self.engine.sample_clock() + bs <= self.last_start_sample {
            self.process_one_block();
        }
        // One more block to drain any FOF sitting exactly at the block boundary.
        self.process_one_block();

        // Render until all active FOFs have decayed (safety limit: 30 s).
        let max_extra = (self.sample_rate as usize) * 30 / self.block_size;
        for _ in 0..max_extra {
            if self.engine.active_count() == 0 {
                break;
            }
            self.process_one_block();
        }
        // SndFile closes on drop.
    }

    fn process_one_block(&mut self) {
        let block_size = self.block_size;
        let n_ch = self.n_channels;

        for buf in &mut self.channel_bufs {
            buf.iter_mut().for_each(|x| *x = 0.0);
        }

        // Split-field borrow: channel_bufs and engine are separate fields.
        {
            let channel_bufs = &mut self.channel_bufs;
            let engine = &mut self.engine;
            let mut slices: Vec<&mut [f32]> =
                channel_bufs.iter_mut().map(|b| b.as_mut_slice()).collect();
            engine.process_block(&mut slices, block_size);
        }

        for s in 0..block_size {
            for ch in 0..n_ch {
                self.interleaved[s * n_ch + ch] = self.channel_bufs[ch][s];
            }
        }

        <SndFile as SndFileIO<f32>>::write_from_slice(
            &mut self.snd,
            &self.interleaved[..block_size * n_ch],
        )
        .expect("sndfile write failed");
    }
}
