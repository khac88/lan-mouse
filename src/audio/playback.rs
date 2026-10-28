use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use cpal::{
    SampleFormat, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

use crate::audio::{
    AudioError,
    jitter::{JitterBuffer, JitterConfig, LinearResampler, f32_to_i16},
};

const REOPEN_RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct PlaybackDeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

struct SharedState {
    jitter: JitterBuffer,
    resampler: LinearResampler,
    desired_rate: u32,
    desired_channels: u8,
}

struct Shared {
    state: Mutex<SharedState>,
    stream_broken: AtomicBool,
    stream_open: AtomicBool,
}

enum PlayerCmd {
    EnsureOpen,
    Reconfigure,
    Shutdown,
}

/// Real-time audio output sink backed by [`JitterBuffer`] and `cpal`.
///
/// Incoming packets are pushed without blocking from the network receive loop;
/// stream creation and recovery (e.g. when switching headphones) run on a
/// dedicated lightweight manager thread so Tokio tasks never block on OS audio
/// initialization.
#[derive(Clone)]
pub struct AudioPlayer {
    shared: Arc<Shared>,
    cmd_tx: mpsc::SyncSender<PlayerCmd>,
    _worker: Arc<WorkerGuard>,
}

struct WorkerGuard {
    cmd_tx: mpsc::SyncSender<PlayerCmd>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PlayerCmd::Shutdown);
        if let Ok(mut guard) = self.handle.lock() {
            if let Some(h) = guard.take() {
                let _ = h.join();
            }
        }
    }
}

impl AudioPlayer {
    /// Creates a lazy player that opens the default output device upon
    /// receiving the first audio packet (and reopens automatically on device
    /// changes).
    pub fn new(buffer_ms: u32) -> Self {
        let initial_rate = 48_000;
        let initial_channels = 2;
        let shared = Arc::new(Shared {
            state: Mutex::new(SharedState {
                jitter: JitterBuffer::new(
                    initial_rate,
                    initial_channels,
                    JitterConfig::with_target_ms(buffer_ms),
                ),
                resampler: LinearResampler::new(initial_rate, initial_rate),
                desired_rate: initial_rate,
                desired_channels: initial_channels,
            }),
            stream_broken: AtomicBool::new(false),
            stream_open: AtomicBool::new(false),
        });

        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<PlayerCmd>(8);
        let worker_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("lan-mouse-audio-out".to_owned())
            .spawn(move || run_player_worker(worker_shared, cmd_rx, None))
            .expect("failed to spawn audio playback worker");

        let guard = Arc::new(WorkerGuard {
            cmd_tx: cmd_tx.clone(),
            handle: Mutex::new(Some(handle)),
        });

        Self {
            shared,
            cmd_tx,
            _worker: guard,
        }
    }

    /// Eagerly opens the default output device immediately on the calling
    /// thread and returns device metadata (used by `test-audio-playback` to
    /// surface device errors synchronously).
    pub fn open_eager(
        sample_rate: u32,
        channels: u8,
        buffer_ms: u32,
    ) -> Result<(Self, PlaybackDeviceInfo), AudioError> {
        let shared = Arc::new(Shared {
            state: Mutex::new(SharedState {
                jitter: JitterBuffer::new(
                    sample_rate,
                    channels,
                    JitterConfig::with_target_ms(buffer_ms),
                ),
                resampler: LinearResampler::new(sample_rate, sample_rate),
                desired_rate: sample_rate,
                desired_channels: channels,
            }),
            stream_broken: AtomicBool::new(false),
            stream_open: AtomicBool::new(false),
        });

        let (stream, info) = open_output_stream(&shared, sample_rate, channels)?;
        shared.stream_open.store(true, Ordering::Release);

        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<PlayerCmd>(8);
        let worker_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("lan-mouse-audio-out".to_owned())
            .spawn(move || run_player_worker(worker_shared, cmd_rx, Some(stream)))
            .expect("failed to spawn audio playback worker");

        let guard = Arc::new(WorkerGuard {
            cmd_tx: cmd_tx.clone(),
            handle: Mutex::new(Some(handle)),
        });

        Ok((
            Self {
                shared,
                cmd_tx,
                _worker: guard,
            },
            info,
        ))
    }

    pub fn set_buffer_ms(&self, buffer_ms: u32) {
        if let Ok(mut guard) = self.shared.state.lock() {
            guard.jitter.set_target_ms(buffer_ms);
        }
    }

    /// Push a decoded PCM packet into the jitter buffer without blocking.
    pub fn push_packet(&self, seq: u32, sample_rate: u32, channels: u8, samples: &[i16]) {
        let mut format_changed = false;
        if let Ok(mut guard) = self.shared.state.lock() {
            if guard.desired_rate != sample_rate || guard.desired_channels != channels {
                guard.desired_rate = sample_rate;
                guard.desired_channels = channels;
                format_changed = true;
            }
            guard.jitter.push(seq, sample_rate, channels, samples);
        }

        if format_changed {
            let _ = self.cmd_tx.try_send(PlayerCmd::Reconfigure);
        } else if !self.shared.stream_open.load(Ordering::Acquire)
            || self.shared.stream_broken.load(Ordering::Acquire)
        {
            let _ = self.cmd_tx.try_send(PlayerCmd::EnsureOpen);
        }
    }
}

fn run_player_worker(
    shared: Arc<Shared>,
    cmd_rx: mpsc::Receiver<PlayerCmd>,
    mut current_stream: Option<cpal::Stream>,
) {
    let mut warned_open_failure = false;

    loop {
        let timeout = if current_stream.is_none() || shared.stream_broken.load(Ordering::Acquire) {
            REOPEN_RETRY_DELAY
        } else {
            Duration::from_secs(3600)
        };

        let mut want_reopen = false;
        match cmd_rx.recv_timeout(timeout) {
            Ok(PlayerCmd::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                drop(current_stream);
                shared.stream_open.store(false, Ordering::Release);
                return;
            }
            Ok(PlayerCmd::EnsureOpen) => {
                if current_stream.is_none() || shared.stream_broken.load(Ordering::Acquire) {
                    want_reopen = true;
                }
            }
            Ok(PlayerCmd::Reconfigure) => {
                want_reopen = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if shared.stream_broken.load(Ordering::Acquire) {
                    want_reopen = true;
                }
            }
        }

        if !want_reopen {
            continue;
        }

        // Drop any invalidated stream before opening a replacement.
        if current_stream.take().is_some() {
            shared.stream_open.store(false, Ordering::Release);
            log::info!("closing previous audio playback stream before reopen");
        }
        shared.stream_broken.store(false, Ordering::Release);

        let (desired_rate, desired_channels) = match shared.state.lock() {
            Ok(g) => (g.desired_rate, g.desired_channels),
            Err(_) => continue,
        };

        match open_output_stream(&shared, desired_rate, desired_channels) {
            Ok((stream, info)) => {
                warned_open_failure = false;
                shared.stream_open.store(true, Ordering::Release);
                log::info!(
                    "audio playback started on '{}' ({} Hz, {} ch, {:?})",
                    info.name,
                    info.sample_rate,
                    info.channels,
                    info.sample_format
                );
                current_stream = Some(stream);
            }
            Err(e) => {
                if !warned_open_failure {
                    log::warn!("failed to open audio output stream (will retry): {e}");
                    warned_open_failure = true;
                } else {
                    log::debug!("audio output reopen attempt failed: {e}");
                }
            }
        }
    }
}

fn select_output_config(
    device: &cpal::Device,
    desired_rate: u32,
    desired_channels: u8,
) -> Result<(SupportedStreamConfig, Option<SupportedStreamConfig>), AudioError> {
    let default_cfg = device.default_output_config()?;
    if default_cfg.sample_rate() == desired_rate
        && default_cfg.channels() == desired_channels as u16
    {
        return Ok((default_cfg, None));
    }

    if let Ok(ranges) = device.supported_output_configs() {
        let mut best = None;
        for range in ranges {
            if range.min_sample_rate() <= desired_rate && desired_rate <= range.max_sample_rate() {
                let fmt_ok = matches!(
                    range.sample_format(),
                    SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
                );
                if !fmt_ok {
                    continue;
                }
                let candidate = range.with_sample_rate(desired_rate);
                let is_better = match &best {
                    None => true,
                    Some(prev) => {
                        score_config(&candidate, desired_channels)
                            > score_config(prev, desired_channels)
                    }
                };
                if is_better {
                    best = Some(candidate);
                }
            }
        }
        if let Some(preferred) = best {
            return Ok((preferred, Some(default_cfg)));
        }
    }

    Ok((default_cfg, None))
}

fn score_config(cfg: &SupportedStreamConfig, desired_channels: u8) -> u8 {
    let ch_score = if cfg.channels() == desired_channels as u16 {
        4
    } else if cfg.channels() == 2 {
        2
    } else {
        0
    };
    let fmt_score = match cfg.sample_format() {
        SampleFormat::F32 => 2,
        SampleFormat::I16 => 1,
        _ => 0,
    };
    ch_score + fmt_score
}

fn open_output_stream(
    shared: &Arc<Shared>,
    desired_rate: u32,
    desired_channels: u8,
) -> Result<(cpal::Stream, PlaybackDeviceInfo), AudioError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioError::NoDefaultOutputDevice)?;

    let name = device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "<unknown>".to_owned());

    let (primary_cfg, fallback_cfg) =
        select_output_config(&device, desired_rate, desired_channels)?;

    let chosen_res = build_stream_with_config(&device, &primary_cfg, shared);
    let (stream, active_cfg) = match (chosen_res, fallback_cfg) {
        (Ok(s), _) => (s, primary_cfg),
        (Err(e), Some(fallback)) => {
            log::debug!(
                "opening output at {} Hz failed ({e}); falling back to device default {} Hz with resampling",
                primary_cfg.sample_rate(),
                fallback.sample_rate()
            );
            let s = build_stream_with_config(&device, &fallback, shared)?;
            (s, fallback)
        }
        (Err(e), None) => return Err(e),
    };

    if let Ok(mut guard) = shared.state.lock() {
        let in_rate = guard.jitter.sample_rate();
        guard.resampler.set_rates(in_rate, active_cfg.sample_rate());
    }

    stream.play()?;

    let info = PlaybackDeviceInfo {
        name,
        sample_rate: active_cfg.sample_rate(),
        channels: active_cfg.channels(),
        sample_format: active_cfg.sample_format(),
    };
    Ok((stream, info))
}

fn build_stream_with_config(
    device: &cpal::Device,
    supported_cfg: &SupportedStreamConfig,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream, AudioError> {
    let sample_format = supported_cfg.sample_format();
    let out_rate = supported_cfg.sample_rate();
    let out_channels = supported_cfg.channels() as usize;
    let stream_cfg: cpal::StreamConfig = (*supported_cfg).into();

    let err_shared = Arc::clone(shared);
    let err_fn = move |err: cpal::Error| {
        log::warn!("audio output stream error: {err}");
        err_shared.stream_broken.store(true, Ordering::Release);
    };

    let data_shared = Arc::clone(shared);
    let timeout = Some(Duration::from_secs(1));

    let stream = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            stream_cfg,
            move |data: &mut [f32], _| {
                if let Ok(mut guard) = data_shared.state.lock() {
                    let SharedState {
                        jitter, resampler, ..
                    } = &mut *guard;
                    resampler.set_rates(jitter.sample_rate(), out_rate);
                    resampler.fill_f32(jitter, out_channels, data);
                } else {
                    data.fill(0.0);
                }
            },
            err_fn,
            timeout,
        )?,
        SampleFormat::I16 => {
            let mut scratch = vec![0.0f32; 4096];
            device.build_output_stream(
                stream_cfg,
                move |data: &mut [i16], _| {
                    if let Ok(mut guard) = data_shared.state.lock() {
                        if guard.jitter.sample_rate() == out_rate
                            && guard.jitter.channels() as usize == out_channels
                        {
                            guard.jitter.read_i16(data);
                            return;
                        }
                        if scratch.len() < data.len() {
                            scratch.resize(data.len(), 0.0);
                        }
                        let SharedState {
                            jitter, resampler, ..
                        } = &mut *guard;
                        resampler.set_rates(jitter.sample_rate(), out_rate);
                        resampler.fill_f32(jitter, out_channels, &mut scratch[..data.len()]);
                        for (dst, &src) in data.iter_mut().zip(scratch.iter()) {
                            *dst = f32_to_i16(src);
                        }
                    } else {
                        data.fill(0);
                    }
                },
                err_fn,
                timeout,
            )?
        }
        SampleFormat::U16 => {
            let mut scratch = vec![0.0f32; 4096];
            device.build_output_stream(
                stream_cfg,
                move |data: &mut [u16], _| {
                    if let Ok(mut guard) = data_shared.state.lock() {
                        if scratch.len() < data.len() {
                            scratch.resize(data.len(), 0.0);
                        }
                        let SharedState {
                            jitter, resampler, ..
                        } = &mut *guard;
                        resampler.set_rates(jitter.sample_rate(), out_rate);
                        resampler.fill_f32(jitter, out_channels, &mut scratch[..data.len()]);
                        for (dst, &src) in data.iter_mut().zip(scratch.iter()) {
                            *dst = (f32_to_i16(src) as i32 + 32768) as u16;
                        }
                    } else {
                        data.fill(32768);
                    }
                },
                err_fn,
                timeout,
            )?
        }
        other => return Err(AudioError::UnsupportedFormat(other)),
    };

    Ok(stream)
}
