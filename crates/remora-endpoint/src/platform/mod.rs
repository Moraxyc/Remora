//! Platform abstraction traits for media capture/rendering, input injection, audio, and clipboard.

pub mod audio;
pub mod clipboard;
pub mod input;
pub mod video;

use std::fmt;

pub use audio::{AudioSink, AudioSource, EncodedAudioFrame};
pub use clipboard::Clipboard;
pub use input::{InputSink, InputSource};
pub use video::{EncodedVideoFrame, VideoSink, VideoSource};

/// Error returned by endpoint platform implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlatformError {
    /// Underlying source reached end-of-stream.
    EndOfStream,
    /// Input or frame bytes failed validation.
    InvalidData(String),
    /// Operating system or file I/O failure.
    Io(String),
    /// Requested capability is unsupported on this platform.
    Unsupported(&'static str),
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndOfStream => f.write_str("platform stream ended"),
            Self::InvalidData(msg) => write!(f, "invalid platform data: {msg}"),
            Self::Io(msg) => write!(f, "platform I/O error: {msg}"),
            Self::Unsupported(cap) => write!(f, "unsupported platform capability: {cap}"),
        }
    }
}

impl std::error::Error for PlatformError {}

impl From<std::io::Error> for PlatformError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

/// Bundle of platform-specific media, input, and clipboard adapters for an endpoint.
pub trait Platform: Send {
    fn video_source(&mut self) -> Option<&mut dyn VideoSource> {
        None
    }

    fn video_sink(&mut self) -> Option<&mut dyn VideoSink> {
        None
    }

    fn input_source(&mut self) -> Option<&mut dyn InputSource> {
        None
    }

    fn input_sink(&mut self) -> Option<&mut dyn InputSink> {
        None
    }

    fn audio_source(&mut self) -> Option<&mut dyn AudioSource> {
        None
    }

    fn audio_sink(&mut self) -> Option<&mut dyn AudioSink> {
        None
    }

    fn clipboard(&mut self) -> Option<&mut dyn Clipboard> {
        None
    }
}
