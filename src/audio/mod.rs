pub mod capture;
pub mod jitter;
pub mod playback;

use cpal::SampleFormat;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no default audio output device found")]
    NoDefaultOutputDevice,
    #[error("unsupported audio sample format: {0:?}")]
    UnsupportedFormat(SampleFormat),
    #[error("{0}")]
    UnsupportedPlatform(&'static str),
    #[error(transparent)]
    Cpal(#[from] cpal::Error),
}
