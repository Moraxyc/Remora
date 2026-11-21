//! Video/audio codec negotiation, capture settings, RTP header extensions, and media
//! `forward_setting` payloads.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::pb;

/// RTP header extension URIs carried on video streams.
pub mod rtp_ext {
    pub const CAPTURE_INDEX_URI: &str =
        "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index";
    pub const NEW_FRAME_URI: &str =
        "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame";
    pub const FEEDBACK_LATENCY_URI: &str =
        "http://www.webrtc.org/experiments/rtp-hdrext/video-feedback-latency";
    pub const FRAME_SENDING_DELAY_URI: &str =
        "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay";
}

/// Encodes the `video-capture-index` RTP extension (2-byte big-endian `u16`).
pub fn encode_capture_index_ext(index: u16) -> [u8; 2] {
    index.to_be_bytes()
}

/// Decodes the `video-capture-index` RTP extension.
pub fn decode_capture_index_ext(bytes: &[u8]) -> Option<u16> {
    let arr: [u8; 2] = bytes.try_into().ok()?;
    Some(u16::from_be_bytes(arr))
}

/// Encodes the `video-frame-is-new-frame` RTP extension (1-byte boolean).
pub fn encode_new_frame_ext(is_new: bool) -> [u8; 1] {
    [u8::from(is_new)]
}

/// Decodes the `video-frame-is-new-frame` RTP extension.
pub fn decode_new_frame_ext(bytes: &[u8]) -> Option<bool> {
    Some(*bytes.first()? != 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VideoCodec {
    H264,
    H265,
    Unknown(i32),
}

impl VideoCodec {
    pub const fn from_wire(code: i32) -> Self {
        match code {
            1 => Self::H264,
            2 => Self::H265,
            other => Self::Unknown(other),
        }
    }

    pub const fn wire_code(self) -> i32 {
        match self {
            Self::H264 => 1,
            Self::H265 => 2,
            Self::Unknown(code) => code,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AudioCodec {
    Opus,
    Unknown(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChromaSampling {
    Yuv420,
    Yuv422,
    Yuv444,
    Yuv400,
    Unknown(i32),
}

impl ChromaSampling {
    pub const fn from_wire(code: i32) -> Self {
        match code {
            1 => Self::Yuv420,
            2 => Self::Yuv422,
            3 => Self::Yuv444,
            4 => Self::Yuv400,
            other => Self::Unknown(other),
        }
    }

    pub const fn wire_code(self) -> i32 {
        match self {
            Self::Yuv420 => 1,
            Self::Yuv422 => 2,
            Self::Yuv444 => 3,
            Self::Yuv400 => 4,
            Self::Unknown(code) => code,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CodecImpl {
    Nvenc,
    AmdAmf,
    IntelQsv,
    VideoToolbox,
    MediaCodec,
    Software,
    Unknown(i32),
}

impl CodecImpl {
    pub const fn from_wire(code: i32) -> Self {
        match code {
            0 => Self::Nvenc,
            1 => Self::AmdAmf,
            2 => Self::IntelQsv,
            3 => Self::VideoToolbox,
            4 => Self::MediaCodec,
            5 => Self::Software,
            other => Self::Unknown(other),
        }
    }

    pub const fn wire_code(self) -> i32 {
        match self {
            Self::Nvenc => 0,
            Self::AmdAmf => 1,
            Self::IntelQsv => 2,
            Self::VideoToolbox => 3,
            Self::MediaCodec => 4,
            Self::Software => 5,
            Self::Unknown(code) => code,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VideoQualityPreset {
    Fast,
    General,
    Hd,
    Bluray,
    Auto,
    Custom,
    Unknown(i32),
}

impl VideoQualityPreset {
    pub const fn from_wire(code: i32) -> Self {
        match code {
            1 => Self::Fast,
            2 => Self::General,
            3 => Self::Hd,
            4 => Self::Bluray,
            5 => Self::Auto,
            6 => Self::Custom,
            other => Self::Unknown(other),
        }
    }

    pub const fn wire_code(self) -> i32 {
        match self {
            Self::Fast => 1,
            Self::General => 2,
            Self::Hd => 3,
            Self::Bluray => 4,
            Self::Auto => 5,
            Self::Custom => 6,
            Self::Unknown(code) => code,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FpsPreset {
    Fps30,
    Fps60,
    Fps90,
    Fps144,
    Unknown(i32),
}

impl FpsPreset {
    pub const fn from_wire(code: i32) -> Self {
        match code {
            1 => Self::Fps30,
            2 => Self::Fps60,
            3 => Self::Fps90,
            4 => Self::Fps144,
            other => Self::Unknown(other),
        }
    }

    pub const fn to_fps(self) -> Option<u32> {
        match self {
            Self::Fps30 => Some(30),
            Self::Fps60 => Some(60),
            Self::Fps90 => Some(90),
            Self::Fps144 => Some(144),
            Self::Unknown(_) => None,
        }
    }
}

/// Normalized capture setting derived from `CaptureSettingRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSetting {
    pub fps: Option<u32>,
    pub quality: VideoQualityPreset,
    pub chroma: ChromaSampling,
    pub max_custom_bitrate_bps: Option<u32>,
    pub cursor_capture: bool,
    pub screen_id: i32,
    pub width: i32,
    pub height: i32,
    pub enable_hdr: bool,
    pub codec: VideoCodec,
}

impl From<&pb::CaptureSettingRequest> for CaptureSetting {
    fn from(req: &pb::CaptureSettingRequest) -> Self {
        let fps = if req.fps_count > 0 {
            u32::try_from(req.fps_count).ok()
        } else {
            FpsPreset::from_wire(req.fps).to_fps()
        };
        let max_custom_bitrate_bps = (req.max_custom_bitrate > 0).then(|| {
            let v = req.max_custom_bitrate as u64;
            let bps = if v < 1_000 {
                v * 1_000_000
            } else if v < 200_000 {
                v * 1_000
            } else {
                v
            };
            u32::try_from(bps).unwrap_or(u32::MAX)
        });
        Self {
            fps,
            quality: VideoQualityPreset::from_wire(req.frame_quality),
            chroma: ChromaSampling::from_wire(req.chorma_format),
            max_custom_bitrate_bps,
            cursor_capture: req.cursor_capture,
            screen_id: req.screen_id,
            width: if req.resolution_pixel_width > 0 {
                req.resolution_pixel_width
            } else {
                req.resolution_width
            },
            height: if req.resolution_pixel_height > 0 {
                req.resolution_pixel_height
            } else {
                req.resolution_height
            },
            enable_hdr: req.enable_hdr,
            codec: VideoCodec::from_wire(req.codec_type),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayCapabilityInfo {
    pub id: i32,
    pub fps: i32,
    #[serde(rename = "type")]
    pub kind: i32,
    pub hdr: i32,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCodecCapability {
    pub video_codec: i32,
    pub chroma_sampling: i32,
    pub codec_impl: i32,
    pub bit_depth: i32,
    pub width: i32,
    pub height: i32,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceCapability {
    pub ice_id: String,
    #[serde(default)]
    pub display_info: Vec<DisplayCapabilityInfo>,
    #[serde(default)]
    pub video_codec_capability: Vec<VideoCodecCapability>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SenderMediaInfo {
    pub capture_impl: String,
    pub encoder_impl: String,
    pub video_track_index: i32,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SenderParaInfo {
    pub ice_id: String,
    pub capture_impl: String,
    pub encoder_impl: String,
    #[serde(default)]
    pub video_track_index: i32,
    #[serde(default)]
    pub sender_media_infos: Vec<SenderMediaInfo>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Decoded media message carried in `forward_setting.data` or codec-negotiation payloads.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum MediaMessage {
    DeviceCapability(DeviceCapability),
    SenderParaInfo(SenderParaInfo),
    Unknown { name: String, raw: Value },
}

/// Decodes the `data` object of a `forward_setting` signaling event without failing on
/// unknown types or extra fields.
pub fn decode_forward_setting(data: &Value) -> MediaMessage {
    let kind = data
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match kind {
        "device_capability" => data
            .get("device_capability")
            .cloned()
            .and_then(|v| serde_json::from_value::<DeviceCapability>(v).ok())
            .map(MediaMessage::DeviceCapability)
            .unwrap_or_else(|| MediaMessage::Unknown {
                name: kind.to_owned(),
                raw: data.clone(),
            }),
        "sender_para_info" => data
            .get("sender_para_info")
            .cloned()
            .and_then(|v| serde_json::from_value::<SenderParaInfo>(v).ok())
            .map(MediaMessage::SenderParaInfo)
            .unwrap_or_else(|| MediaMessage::Unknown {
                name: kind.to_owned(),
                raw: data.clone(),
            }),
        other => MediaMessage::Unknown {
            name: other.to_owned(),
            raw: data.clone(),
        },
    }
}

/// Encodes a [`MediaMessage`] into the nested `data` object of `forward_setting`.
pub fn encode_forward_setting(msg: &MediaMessage) -> Value {
    match msg {
        MediaMessage::DeviceCapability(cap) => json!({
            "type": "device_capability",
            "device_capability": cap,
        }),
        MediaMessage::SenderParaInfo(info) => json!({
            "type": "sender_para_info",
            "sender_para_info": info,
        }),
        MediaMessage::Unknown { raw, .. } => raw.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_extensions_roundtrip() {
        let idx = encode_capture_index_ext(3);
        assert_eq!(idx, [0x00, 0x03]);
        assert_eq!(decode_capture_index_ext(&idx), Some(3));
        assert_eq!(decode_capture_index_ext(&[0x00]), None);

        let nf = encode_new_frame_ext(true);
        assert_eq!(nf, [0x01]);
        assert_eq!(decode_new_frame_ext(&nf), Some(true));
        assert_eq!(decode_new_frame_ext(&[0x00]), Some(false));
    }

    #[test]
    fn forward_setting_roundtrips_and_tolerates_unknown() {
        let cap = MediaMessage::DeviceCapability(DeviceCapability {
            ice_id: "ice-1".into(),
            display_info: vec![DisplayCapabilityInfo {
                id: 0,
                fps: 60,
                kind: 0,
                hdr: -1,
                extra: Map::new(),
            }],
            video_codec_capability: vec![VideoCodecCapability {
                video_codec: VideoCodec::H264.wire_code(),
                chroma_sampling: ChromaSampling::Yuv420.wire_code(),
                codec_impl: CodecImpl::VideoToolbox.wire_code(),
                bit_depth: 8,
                width: 3840,
                height: 2160,
                extra: Map::new(),
            }],
            extra: Map::new(),
        });
        let encoded = encode_forward_setting(&cap);
        assert_eq!(decode_forward_setting(&encoded), cap);

        let unknown = json!({ "type": "future_media_hint", "value": 42 });
        assert_eq!(
            decode_forward_setting(&unknown),
            MediaMessage::Unknown {
                name: "future_media_hint".into(),
                raw: unknown,
            }
        );
    }
}
