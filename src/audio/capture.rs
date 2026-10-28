use std::time::Duration;

use cpal::{
    SampleFormat,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use tokio::sync::mpsc;

use crate::audio::{
    AudioError,
    jitter::{FrameChunker, PcmChunk, i32_to_f32, u16_to_f32},
};

#[derive(Clone, Debug)]
pub struct CaptureDeviceInfo {
    pub name: String,
    pub native_sample_rate: u32,
    pub native_channels: u16,
    pub native_sample_format: SampleFormat,
    pub out_sample_rate: u32,
    pub out_channels: u8,
}

#[derive(Debug)]
pub enum CaptureMessage {
    Chunk(PcmChunk),
    StreamError(String),
}

pub struct LoopbackCapture {
    stream: cpal::Stream,
    info: CaptureDeviceInfo,
}

impl LoopbackCapture {
    /// Opens a WASAPI loopback stream on the system's default output device.
    ///
    /// On Windows, `cpal` transparently enables `AUDCLNT_STREAMFLAGS_LOOPBACK`
    /// when building an input stream on a render (`default_output_device`)
    /// endpoint. Non-Windows platforms return [`AudioError::UnsupportedPlatform`]
    /// so we never unexpectedly create virtual aggregate devices or trigger OS
    /// recording permission prompts.
    pub fn open(tx: mpsc::Sender<CaptureMessage>) -> Result<Self, AudioError> {
        if !cfg!(windows) {
            return Err(AudioError::UnsupportedPlatform(
                "audio loopback capture is only supported on Windows",
            ));
        }

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioError::NoDefaultOutputDevice)?;

        let name = device
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_else(|_| "<unknown>".to_owned());

        let supported_cfg = device.default_output_config()?;
        let native_sample_rate = supported_cfg.sample_rate();
        let native_channels = supported_cfg.channels();
        let native_sample_format = supported_cfg.sample_format();
        let stream_cfg: cpal::StreamConfig = supported_cfg.into();

        let mut chunker = FrameChunker::new(native_sample_rate, native_channels);
        let out_sample_rate = chunker.out_sample_rate();
        let out_channels = chunker.out_channels();

        let err_tx = tx.clone();
        let err_fn = move |err: cpal::Error| match err.kind() {
            cpal::ErrorKind::Xrun
            | cpal::ErrorKind::DeviceChanged
            | cpal::ErrorKind::RealtimeDenied => {
                log::debug!(
                    "non-fatal audio loopback notification ({:?}): {err}",
                    err.kind()
                );
            }
            _ => {
                let _ = err_tx.try_send(CaptureMessage::StreamError(err.to_string()));
            }
        };

        let timeout = Some(Duration::from_secs(2));
        let stream = match native_sample_format {
            SampleFormat::F32 => device.build_input_stream(
                stream_cfg,
                move |data: &[f32], _| {
                    chunker.push_f32(data, |chunk| {
                        let _ = tx.try_send(CaptureMessage::Chunk(chunk));
                    });
                },
                err_fn,
                timeout,
            )?,
            SampleFormat::I16 => device.build_input_stream(
                stream_cfg,
                move |data: &[i16], _| {
                    chunker.push_i16(data, |chunk| {
                        let _ = tx.try_send(CaptureMessage::Chunk(chunk));
                    });
                },
                err_fn,
                timeout,
            )?,
            SampleFormat::U16 => {
                let mut scratch = Vec::new();
                device.build_input_stream(
                    stream_cfg,
                    move |data: &[u16], _| {
                        scratch.clear();
                        scratch.extend(data.iter().copied().map(u16_to_f32));
                        chunker.push_f32(&scratch, |chunk| {
                            let _ = tx.try_send(CaptureMessage::Chunk(chunk));
                        });
                    },
                    err_fn,
                    timeout,
                )?
            }
            SampleFormat::I32 => {
                let mut scratch = Vec::new();
                device.build_input_stream(
                    stream_cfg,
                    move |data: &[i32], _| {
                        scratch.clear();
                        scratch.extend(data.iter().copied().map(i32_to_f32));
                        chunker.push_f32(&scratch, |chunk| {
                            let _ = tx.try_send(CaptureMessage::Chunk(chunk));
                        });
                    },
                    err_fn,
                    timeout,
                )?
            }
            other => return Err(AudioError::UnsupportedFormat(other)),
        };

        let info = CaptureDeviceInfo {
            name,
            native_sample_rate,
            native_channels,
            native_sample_format,
            out_sample_rate,
            out_channels,
        };

        Ok(Self { stream, info })
    }

    pub fn play(&self) -> Result<(), AudioError> {
        self.stream.play()?;
        Ok(())
    }

    pub fn pause(&self) -> Result<(), AudioError> {
        self.stream.pause()?;
        Ok(())
    }

    pub fn info(&self) -> &CaptureDeviceInfo {
        &self.info
    }
}
