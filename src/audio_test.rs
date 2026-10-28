use std::{
    f64::consts::PI,
    time::{Duration, Instant},
};

use clap::Args;
use tokio::sync::mpsc;

use crate::audio::{
    AudioError,
    capture::{CaptureMessage, LoopbackCapture},
    jitter::{DEFAULT_BUFFER_MS, FRAME_DURATION_MS, f32_to_i16, i16_to_f32},
    playback::AudioPlayer,
};

#[derive(Args, Clone, Debug, Eq, PartialEq)]
pub struct TestAudioCaptureArgs {
    /// capture duration in seconds
    #[arg(long, default_value_t = 5)]
    pub duration_secs: u64,
}

#[derive(Args, Clone, Debug)]
pub struct TestAudioPlaybackArgs {
    /// playback duration in seconds
    #[arg(long, default_value_t = 2)]
    pub duration_secs: u64,
    /// sine wave frequency in Hz
    #[arg(long, default_value_t = 440.0)]
    pub freq_hz: f64,
}

impl PartialEq for TestAudioPlaybackArgs {
    fn eq(&self, other: &Self) -> bool {
        self.duration_secs == other.duration_secs
            && self.freq_hz.to_bits() == other.freq_hz.to_bits()
    }
}

impl Eq for TestAudioPlaybackArgs {}

pub async fn run_capture(args: TestAudioCaptureArgs) -> Result<(), AudioError> {
    let (tx, mut rx) = mpsc::channel(128);
    let capture = LoopbackCapture::open(tx)?;
    let info = capture.info().clone();

    log::info!(
        "opened loopback capture on '{}': native={} Hz {} ch {:?} -> wire={} Hz {} ch s16le",
        info.name,
        info.native_sample_rate,
        info.native_channels,
        info.native_sample_format,
        info.out_sample_rate,
        info.out_channels,
    );
    println!(
        "Device: {} (native: {} Hz, {} ch, {:?} -> wire: {} Hz, {} ch, s16le)",
        info.name,
        info.native_sample_rate,
        info.native_channels,
        info.native_sample_format,
        info.out_sample_rate,
        info.out_channels,
    );

    capture.play()?;

    let duration = Duration::from_secs(args.duration_secs);
    let deadline = tokio::time::Instant::now() + duration;

    let mut chunks = 0u64;
    let mut total_frames = 0u64;
    let mut sum_sq = 0.0f64;
    let mut total_samples = 0u64;
    let mut peak = 0.0f32;

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            msg = rx.recv() => match msg {
                Some(CaptureMessage::Chunk(chunk)) => {
                    chunks += 1;
                    total_frames += (chunk.samples.len() / chunk.channels as usize) as u64;
                    for &s in &chunk.samples {
                        let f = i16_to_f32(s);
                        let abs = f.abs();
                        if abs > peak {
                            peak = abs;
                        }
                        sum_sq += (f as f64) * (f as f64);
                        total_samples += 1;
                    }
                }
                Some(CaptureMessage::StreamError(err)) => {
                    log::warn!("loopback stream error during test: {err}");
                }
                None => break,
            }
        }
    }

    let _ = capture.pause();

    let rms = if total_samples > 0 {
        (sum_sq / total_samples as f64).sqrt()
    } else {
        0.0
    };
    let rms_dbfs = if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        f64::NEG_INFINITY
    };

    println!(
        "Captured {}s: {} chunks ({} PCM frames), RMS={:.6} ({:.1} dBFS), peak={:.6}",
        args.duration_secs, chunks, total_frames, rms, rms_dbfs, peak
    );
    log::info!(
        "capture summary: chunks={chunks}, frames={total_frames}, rms={rms:.6} ({rms_dbfs:.1} dBFS), peak={peak:.6}"
    );
    Ok(())
}

pub async fn run_playback(args: TestAudioPlaybackArgs) -> Result<(), AudioError> {
    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: u8 = 2;
    const AMPLITUDE: f64 = 0.25;

    let (player, info) = AudioPlayer::open_eager(SAMPLE_RATE, CHANNELS, DEFAULT_BUFFER_MS)?;
    log::info!(
        "playing {:.1} Hz sine for {}s via {} ms jitter buffer on '{}' ({} Hz, {} ch, {:?})",
        args.freq_hz,
        args.duration_secs,
        DEFAULT_BUFFER_MS,
        info.name,
        info.sample_rate,
        info.channels,
        info.sample_format,
    );

    let frames_per_packet = (SAMPLE_RATE as usize * FRAME_DURATION_MS as usize) / 1000;
    let total_packets = (args.duration_secs * 1000) / FRAME_DURATION_MS as u64;
    let mut phase = 0.0f64;
    let phase_inc = 2.0 * PI * args.freq_hz / SAMPLE_RATE as f64;

    let start = Instant::now();
    for seq in 0..total_packets {
        let mut samples = Vec::with_capacity(frames_per_packet * CHANNELS as usize);
        for _ in 0..frames_per_packet {
            let s = f32_to_i16((phase.sin() * AMPLITUDE) as f32);
            samples.push(s);
            samples.push(s);
            phase = (phase + phase_inc).rem_euclid(2.0 * PI);
        }
        player.push_packet(seq as u32, SAMPLE_RATE, CHANNELS, &samples);

        let next_tick = Duration::from_millis((seq + 1) * FRAME_DURATION_MS as u64);
        if let Some(rem) = next_tick.checked_sub(start.elapsed()) {
            tokio::time::sleep(rem).await;
        }
    }

    // Allow the jitter buffer (30 ms target + hardware latency) to finish
    // draining cleanly through its 5 ms fade-out before dropping the stream.
    tokio::time::sleep(Duration::from_millis((DEFAULT_BUFFER_MS + 60) as u64)).await;
    log::info!("audio playback test complete ({total_packets} packets sent)");
    Ok(())
}
