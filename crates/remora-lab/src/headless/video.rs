//! H.264 Annex-B file video source and validating frame-counting sink.

use std::fs;
use std::path::Path;

use remora_endpoint::platform::{EncodedVideoFrame, PlatformError, VideoSink, VideoSource};
use remora_proto::session::media::VideoCodec;

/// Default frame interval (30 fps) used when assigning monotonic timestamps to Annex-B units.
const DEFAULT_FRAME_STEP_MS: u64 = 33;

/// Reads pre-encoded H.264 Annex-B NALUs from a byte stream or `.h264` file and emits frames.
#[derive(Debug, Clone)]
pub struct AnnexBFileVideoSource {
    frames: Vec<EncodedVideoFrame>,
    cursor: usize,
    looping: bool,
}

impl AnnexBFileVideoSource {
    /// Opens and parses an H.264 Annex-B file from `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PlatformError> {
        let bytes = fs::read(path)?;
        Self::from_bytes(&bytes)
    }

    /// Parses an in-memory H.264 Annex-B byte stream into frames.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PlatformError> {
        let frames = parse_annex_b_frames(bytes, DEFAULT_FRAME_STEP_MS)?;
        Ok(Self {
            frames,
            cursor: 0,
            looping: false,
        })
    }

    /// Enables or disables looping once all frames have been read.
    pub fn with_loop(mut self, looping: bool) -> Self {
        self.looping = looping;
        self
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

impl VideoSource for AnnexBFileVideoSource {
    fn next_frame(&mut self) -> Result<Option<EncodedVideoFrame>, PlatformError> {
        if self.frames.is_empty() {
            return Ok(None);
        }
        if self.cursor >= self.frames.len() {
            if self.looping {
                self.cursor = 0;
            } else {
                return Ok(None);
            }
        }
        let frame = self.frames[self.cursor].clone();
        self.cursor += 1;
        Ok(Some(frame))
    }
}

/// Validates received encoded frames and records frame/keyframe/byte statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CountingVideoSink {
    pub frames: u64,
    pub keyframes: u64,
    pub bytes: u64,
    pub last_pts_ms: Option<u64>,
}

impl CountingVideoSink {
    pub fn new() -> Self {
        Self::default()
    }
}

impl VideoSink for CountingVideoSink {
    fn push_frame(&mut self, frame: &EncodedVideoFrame) -> Result<(), PlatformError> {
        if frame.data.is_empty() {
            return Err(PlatformError::InvalidData("empty video frame".into()));
        }
        let nalus = split_annex_b_nalus(&frame.data)?;
        if nalus.is_empty() {
            return Err(PlatformError::InvalidData(
                "video frame contains no Annex-B NALUs".into(),
            ));
        }
        let mut saw_idr = false;
        for nalu in nalus {
            let header = *nalu
                .first()
                .ok_or_else(|| PlatformError::InvalidData("empty NAL unit".into()))?;
            // H.264 NAL header: forbidden_zero_bit (bit 7) must be 0, nal_unit_type in bits 0..=4.
            if (header & 0x80) != 0 {
                return Err(PlatformError::InvalidData(
                    "NALU forbidden_zero_bit is non-zero".into(),
                ));
            }
            let nal_type = header & 0x1F;
            if nal_type == 0 {
                return Err(PlatformError::InvalidData("invalid NALU type 0".into()));
            }
            if nal_type == 5 {
                saw_idr = true;
            }
        }
        if frame.is_keyframe && !saw_idr && frame.codec == VideoCodec::H264 {
            return Err(PlatformError::InvalidData(
                "keyframe flag set without IDR slice".into(),
            ));
        }

        self.frames += 1;
        if frame.is_keyframe {
            self.keyframes += 1;
        }
        self.bytes += frame.data.len() as u64;
        self.last_pts_ms = Some(frame.pts_ms);
        Ok(())
    }
}

/// Splits an Annex-B byte stream into NALU slices (without start codes).
pub fn split_annex_b_nalus(bytes: &[u8]) -> Result<Vec<&[u8]>, PlatformError> {
    let mut offsets = Vec::new();
    let mut i = 0usize;
    while i + 3 <= bytes.len() {
        if i + 4 <= bytes.len() && bytes[i..i + 4] == [0, 0, 0, 1] {
            offsets.push((i, 4usize));
            i += 4;
        } else if bytes[i..i + 3] == [0, 0, 1] {
            offsets.push((i, 3usize));
            i += 3;
        } else {
            i += 1;
        }
    }
    if offsets.is_empty() {
        return Err(PlatformError::InvalidData(
            "missing Annex-B start code".into(),
        ));
    }
    if offsets[0].0 != 0 {
        return Err(PlatformError::InvalidData(
            "Annex-B stream does not begin with a start code".into(),
        ));
    }
    let mut nalus = Vec::with_capacity(offsets.len());
    for idx in 0..offsets.len() {
        let start = offsets[idx].0 + offsets[idx].1;
        let end = offsets
            .get(idx + 1)
            .map_or(bytes.len(), |(next_start, _)| *next_start);
        if start < end {
            nalus.push(&bytes[start..end]);
        }
    }
    Ok(nalus)
}

/// Parses H.264 Annex-B NALUs into access units (frames), attaching leading SPS/PPS/SEI/AUD to
/// the following VCL slice.
pub fn parse_annex_b_frames(
    bytes: &[u8],
    frame_step_ms: u64,
) -> Result<Vec<EncodedVideoFrame>, PlatformError> {
    let nalus = split_annex_b_nalus(bytes)?;
    let mut frames = Vec::new();
    let mut pending_prefix: Vec<u8> = Vec::new();

    for nalu in nalus {
        let header = *nalu
            .first()
            .ok_or_else(|| PlatformError::InvalidData("empty NALU".into()))?;
        if (header & 0x80) != 0 {
            return Err(PlatformError::InvalidData(
                "NALU forbidden_zero_bit is non-zero".into(),
            ));
        }
        let nal_type = header & 0x1F;
        match nal_type {
            // Non-VCL parameter / delimiter NALUs attach to the next slice.
            6..=9 => {
                pending_prefix.extend_from_slice(&[0, 0, 0, 1]);
                pending_prefix.extend_from_slice(nalu);
            }
            // VCL slice (1 = non-IDR, 5 = IDR).
            1..=5 => {
                let is_keyframe = nal_type == 5;
                let mut data = Vec::with_capacity(pending_prefix.len() + 4 + nalu.len());
                data.append(&mut pending_prefix);
                data.extend_from_slice(&[0, 0, 0, 1]);
                data.extend_from_slice(nalu);
                let pts_ms = (frames.len() as u64) * frame_step_ms;
                frames.push(EncodedVideoFrame {
                    codec: VideoCodec::H264,
                    is_keyframe,
                    pts_ms,
                    data,
                });
            }
            0 => {
                return Err(PlatformError::InvalidData("invalid NALU type 0".into()));
            }
            _ => {
                pending_prefix.extend_from_slice(&[0, 0, 0, 1]);
                pending_prefix.extend_from_slice(nalu);
            }
        }
    }

    if frames.is_empty() {
        return Err(PlatformError::InvalidData(
            "Annex-B stream contains no VCL video frames".into(),
        ));
    }
    Ok(frames)
}

/// Returns a small synthetic H.264 Annex-B byte stream containing SPS, PPS, one IDR keyframe,
/// and two non-IDR frames for headless tests.
pub fn synthetic_annex_b_sample() -> Vec<u8> {
    let mut out = Vec::new();
    // SPS (NAL type 7)
    out.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x47]);
    // PPS (NAL type 8)
    out.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80]);
    // IDR slice (NAL type 5)
    out.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21, 0xA0]);
    // Non-IDR P-slice (NAL type 1)
    out.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x24, 0x6C]);
    // Second non-IDR P-slice (NAL type 1)
    out.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x24, 0x6D]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_annex_b_file_and_counts_validated_frames() {
        let dir = std::env::temp_dir().join(format!("remora-lab-video-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.h264");
        fs::write(&path, synthetic_annex_b_sample()).unwrap();

        let mut source = AnnexBFileVideoSource::open(&path).unwrap();
        assert_eq!(source.len(), 3);

        let mut sink = CountingVideoSink::new();
        while let Some(frame) = source.next_frame().unwrap() {
            sink.push_frame(&frame).unwrap();
        }
        assert_eq!(sink.frames, 3);
        assert_eq!(sink.keyframes, 1);
        assert!(sink.bytes > 0);
        assert_eq!(sink.last_pts_ms, Some(66));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sink_rejects_corrupt_annex_b_frames() {
        let mut sink = CountingVideoSink::new();
        let bad_start = EncodedVideoFrame {
            codec: VideoCodec::H264,
            is_keyframe: false,
            pts_ms: 0,
            data: vec![1, 2, 3, 4],
        };
        assert!(sink.push_frame(&bad_start).is_err());

        let bad_idr = EncodedVideoFrame {
            codec: VideoCodec::H264,
            is_keyframe: true,
            pts_ms: 0,
            data: vec![0, 0, 0, 1, 0x41, 0x9A],
        };
        assert!(sink.push_frame(&bad_idr).is_err());
        assert_eq!(sink.frames, 0);
    }
}
