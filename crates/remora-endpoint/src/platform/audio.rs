//! Encoded audio frame source and sink traits.

use remora_proto::session::media::AudioCodec;

use super::{Bytes, PlatformError};

/// One encoded audio frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAudioFrame {
    pub codec: AudioCodec,
    pub pts_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub data: Bytes,
}

/// Produces encoded audio frames.
pub trait AudioSource: Send {
    fn next_frame(&mut self) -> Result<Option<EncodedAudioFrame>, PlatformError>;
}

/// Consumes encoded audio frames.
pub trait AudioSink: Send {
    fn push_frame(&mut self, frame: &EncodedAudioFrame) -> Result<(), PlatformError>;
}
