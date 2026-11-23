//! Headless audio source and sink.

use remora_endpoint::platform::{AudioSink, AudioSource, Bytes, EncodedAudioFrame, PlatformError};
use remora_proto::session::media::AudioCodec;

/// Emits a finite number of empty Opus placeholder frames.
#[derive(Debug, Clone, Default)]
pub struct SilenceAudioSource {
    remaining: u32,
    pts_ms: u64,
}

impl SilenceAudioSource {
    pub fn new(frames: u32) -> Self {
        Self {
            remaining: frames,
            pts_ms: 0,
        }
    }
}

impl AudioSource for SilenceAudioSource {
    fn next_frame(&mut self) -> Result<Option<EncodedAudioFrame>, PlatformError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let pts_ms = self.pts_ms;
        self.pts_ms += 20;
        Ok(Some(EncodedAudioFrame {
            codec: AudioCodec::Opus,
            pts_ms,
            sample_rate: 48_000,
            channels: 2,
            data: Bytes::from_static(&[0xF8, 0xFF, 0xFE]),
        }))
    }
}

/// Counts received audio frames.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CountingAudioSink {
    pub frames: u64,
    pub bytes: u64,
}

impl AudioSink for CountingAudioSink {
    fn push_frame(&mut self, frame: &EncodedAudioFrame) -> Result<(), PlatformError> {
        if frame.data.is_empty() {
            return Err(PlatformError::InvalidData("empty audio frame".into()));
        }
        self.frames += 1;
        self.bytes += frame.data.len() as u64;
        Ok(())
    }
}
