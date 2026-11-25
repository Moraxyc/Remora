//! Encoded video frame source and sink traits.

use remora_proto::session::media::VideoCodec;

use super::{Bytes, PlatformError};

/// One encoded video access unit (for H.264/H.265, Annex-B byte stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVideoFrame {
    pub codec: VideoCodec,
    pub is_keyframe: bool,
    pub pts_ms: u64,
    pub capture_index: Option<u16>,
    pub is_new_frame: Option<bool>,
    pub data: Bytes,
}

impl EncodedVideoFrame {
    pub fn new(codec: VideoCodec, is_keyframe: bool, pts_ms: u64, data: Bytes) -> Self {
        Self {
            codec,
            is_keyframe,
            pts_ms,
            capture_index: Some(0),
            is_new_frame: Some(true),
            data,
        }
    }
}

/// Produces encoded video frames for outbound streaming.
pub trait VideoSource: Send {
    /// Returns the next encoded frame, or `None` when the source is exhausted.
    fn next_frame(&mut self) -> Result<Option<EncodedVideoFrame>, PlatformError>;
}

/// Consumes encoded video frames received from a remote peer.
pub trait VideoSink: Send {
    fn push_frame(&mut self, frame: &EncodedVideoFrame) -> Result<(), PlatformError>;
}
