//! Bridges `str0m` WebRTC data channels and media tracks with `remora_proto::session` codecs
//! and [`crate::platform`] frame types.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use prost::encoding::{WireType, decode_key, skip_field};
use remora_proto::pb::{self, rpc_request};
use remora_proto::session::DataChannel;
use remora_proto::session::channels as official_channels;
use remora_proto::session::control::{ControlMessage, encode_message};
use remora_proto::session::media::{AudioCodec, VideoCodec};
use str0m::Rtc;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaData, MediaTime, Mid, Pt};

use crate::platform::{Bytes, EncodedAudioFrame, EncodedVideoFrame};

/// Standard UU data channel labels opened by the viewer in order.
pub const DEFAULT_DATA_CHANNELS: [&str; 5] = [
    official_channels::CONTROL,
    official_channels::TEXT,
    official_channels::STREAMER,
    official_channels::FILE,
    official_channels::BINARY,
];

/// Error returned while encoding or writing media / data-channel payloads on a `str0m` peer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChannelError {
    ChannelNotOpen(String),
    NoVideoTrack,
    NoAudioTrack,
    UnsupportedCodec(String),
    BufferFull(String),
    Rtc(String),
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChannelNotOpen(label) => write!(f, "data channel `{label}` is not open"),
            Self::NoVideoTrack => f.write_str("no negotiated outbound video track"),
            Self::NoAudioTrack => f.write_str("no negotiated outbound audio track"),
            Self::UnsupportedCodec(name) => write!(f, "no negotiated payload type for `{name}`"),
            Self::BufferFull(label) => write!(f, "SCTP send buffer full on `{label}`"),
            Self::Rtc(msg) => write!(f, "WebRTC channel/media write error: {msg}"),
        }
    }
}

impl std::error::Error for ChannelError {}

/// Tracks open `str0m` data channels and negotiated media `Mid`s for one peer.
#[derive(Debug, Default)]
pub struct ChannelRegistry {
    label_to_id: HashMap<DataChannel, ChannelId>,
    id_to_label: HashMap<ChannelId, DataChannel>,
    video_tx_mid: Option<Mid>,
    video_rx_mid: Option<Mid>,
    audio_tx_mid: Option<Mid>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_channel(&mut self, id: ChannelId, label: &str) -> DataChannel {
        let ch = DataChannel::from_label(label);
        self.label_to_id.insert(ch.clone(), id);
        self.id_to_label.insert(id, ch.clone());
        ch
    }

    pub fn unregister_channel(&mut self, id: ChannelId) -> Option<DataChannel> {
        let ch = self.id_to_label.remove(&id)?;
        self.label_to_id.remove(&ch);
        Some(ch)
    }

    pub fn lookup(&self, id: ChannelId) -> Option<&DataChannel> {
        self.id_to_label.get(&id)
    }

    pub fn channel_id(&self, ch: &DataChannel) -> Option<ChannelId> {
        self.label_to_id.get(ch).copied()
    }

    pub fn set_video_tx_mid(&mut self, mid: Mid) {
        self.video_tx_mid = Some(mid);
    }

    pub fn video_tx_mid(&self) -> Option<Mid> {
        self.video_tx_mid
    }

    pub fn set_audio_tx_mid(&mut self, mid: Mid) {
        self.audio_tx_mid = Some(mid);
    }

    pub fn audio_tx_mid(&self) -> Option<Mid> {
        self.audio_tx_mid
    }

    pub fn set_video_rx_mid(&mut self, mid: Mid) {
        self.video_rx_mid = Some(mid);
    }

    pub fn video_rx_mid(&self) -> Option<Mid> {
        self.video_rx_mid
    }
}

/// Writes `data` onto `channel` on `rtc`.
///
/// JSON input payloads (starting with `{`) are sent as text frames (`binary = false`);
/// protobuf envelopes and opaque binary traffic are sent as binary frames (`binary = true`).
pub fn write_channel(
    rtc: &mut Rtc,
    registry: &ChannelRegistry,
    channel: &DataChannel,
    data: &[u8],
) -> Result<(), ChannelError> {
    let id = registry
        .channel_id(channel)
        .ok_or_else(|| ChannelError::ChannelNotOpen(channel.as_str().to_owned()))?;
    let mut ch = rtc
        .channel(id)
        .ok_or_else(|| ChannelError::ChannelNotOpen(channel.as_str().to_owned()))?;
    let binary = data.first() != Some(&b'{');
    let accepted = ch
        .write(binary, data)
        .map_err(|e| ChannelError::Rtc(e.to_string()))?;
    if !accepted {
        return Err(ChannelError::BufferFull(channel.as_str().to_owned()));
    }
    Ok(())
}

/// Writes one encoded video access unit (`EncodedVideoFrame`) directly onto the negotiated
/// `str0m` video track.
pub fn write_video_frame(
    rtc: &mut Rtc,
    registry: &ChannelRegistry,
    wallclock_anchor: Instant,
    frame: &EncodedVideoFrame,
) -> Result<(), ChannelError> {
    let mid = registry.video_tx_mid().ok_or(ChannelError::NoVideoTrack)?;
    let target_codec = match frame.codec {
        VideoCodec::H264 => Codec::H264,
        VideoCodec::H265 => Codec::H265,
        VideoCodec::Unknown(code) => {
            return Err(ChannelError::UnsupportedCodec(format!("video:{code}")));
        }
        other => {
            return Err(ChannelError::UnsupportedCodec(format!("{other:?}")));
        }
    };
    let writer = rtc.writer(mid).ok_or(ChannelError::NoVideoTrack)?;
    let pt = select_video_pt(writer.payload_params(), target_codec)
        .ok_or_else(|| ChannelError::UnsupportedCodec(format!("{:?}", frame.codec)))?;
    let wallclock = wallclock_anchor + Duration::from_millis(frame.pts_ms);
    let media_time = MediaTime::new(frame.pts_ms.saturating_mul(90), Frequency::NINETY_KHZ);
    writer
        .write(pt, wallclock, media_time, frame.data.as_ref())
        .map_err(|e| ChannelError::Rtc(e.to_string()))?;
    Ok(())
}

fn select_video_pt<'a>(
    params: impl Iterator<Item = &'a str0m::format::PayloadParams>,
    target: Codec,
) -> Option<Pt> {
    let mut fallback = None;
    for p in params {
        if p.spec().codec != target {
            continue;
        }
        // Prefer packetization-mode=1 for H.264 when multiple payload types were negotiated.
        if p.spec().format.packetization_mode == Some(1) {
            return Some(p.pt());
        }
        if fallback.is_none() {
            fallback = Some(p.pt());
        }
    }
    fallback
}

/// Writes one encoded audio frame (`EncodedAudioFrame`) onto the negotiated `str0m` audio track.
pub fn write_audio_frame(
    rtc: &mut Rtc,
    registry: &ChannelRegistry,
    wallclock_anchor: Instant,
    frame: &EncodedAudioFrame,
) -> Result<(), ChannelError> {
    let mid = registry.audio_tx_mid().ok_or(ChannelError::NoAudioTrack)?;
    let target_codec = match frame.codec {
        AudioCodec::Opus => Codec::Opus,
        AudioCodec::Unknown(code) => {
            return Err(ChannelError::UnsupportedCodec(format!("audio:{code}")));
        }
        other => {
            return Err(ChannelError::UnsupportedCodec(format!("{other:?}")));
        }
    };
    let writer = rtc.writer(mid).ok_or(ChannelError::NoAudioTrack)?;
    let pt = writer
        .payload_params()
        .find(|p| p.spec().codec == target_codec)
        .map(|p| p.pt())
        .ok_or_else(|| ChannelError::UnsupportedCodec("opus".into()))?;
    let wallclock = wallclock_anchor + Duration::from_millis(frame.pts_ms);
    let media_time = MediaTime::new(frame.pts_ms.saturating_mul(48), Frequency::FORTY_EIGHT_KHZ);
    writer
        .write(pt, wallclock, media_time, frame.data.as_ref())
        .map_err(|e| ChannelError::Rtc(e.to_string()))?;
    Ok(())
}

/// Converts a depayloaded `str0m` [`MediaData`] video frame into [`EncodedVideoFrame`].
pub fn media_data_to_video_frame(data: &MediaData) -> Option<EncodedVideoFrame> {
    let codec = match data.params.spec().codec {
        Codec::H264 => VideoCodec::H264,
        Codec::H265 => VideoCodec::H265,
        _ => return None,
    };
    let pts_ms = data.time.rebase(Frequency::MILLIS).numer();
    let is_keyframe = data.is_keyframe() || contains_annex_b_keyframe(codec, &data.data);
    Some(EncodedVideoFrame {
        codec,
        is_keyframe,
        pts_ms,
        data: Bytes::from_owner(Arc::clone(&data.data)),
    })
}

/// Converts a depayloaded `str0m` [`MediaData`] audio frame into [`EncodedAudioFrame`].
pub fn media_data_to_audio_frame(data: &MediaData) -> Option<EncodedAudioFrame> {
    if data.params.spec().codec != Codec::Opus {
        return None;
    }
    let pts_ms = data.time.rebase(Frequency::MILLIS).numer();
    let channels = u16::from(data.params.spec().channels.unwrap_or(2));
    Some(EncodedAudioFrame {
        codec: AudioCodec::Opus,
        pts_ms,
        sample_rate: data.params.spec().clock_rate.get(),
        channels,
        data: Bytes::from_owner(Arc::clone(&data.data)),
    })
}

/// Returns up to 32 top-level field numbers and wire types, stopping at a malformed field or group.
pub fn inspect_protobuf_shape(mut bytes: &[u8]) -> Vec<(u32, u8)> {
    const MAX_FIELDS: usize = 32;
    let mut fields = Vec::new();
    while fields.len() < MAX_FIELDS && !bytes.is_empty() {
        let Ok((number, wire_type)) = decode_key(&mut bytes) else {
            break;
        };
        if matches!(wire_type, WireType::StartGroup | WireType::EndGroup)
            || skip_field(wire_type, number, &mut bytes, Default::default()).is_err()
        {
            break;
        }
        fields.push((number, wire_type as u8));
    }
    fields
}

/// Builds the host initialization burst sent when `CONTROL_DATA_CHANNEL` opens.
pub fn build_host_init_burst(
    mut alloc_seq: impl FnMut() -> i64,
    timestamp_ms: i64,
    width: i32,
    height: i32,
    fps: i32,
    support_hevc: bool,
    _clipboard_enabled: bool,
) -> Vec<Vec<u8>> {
    let rect = pb::WinRect {
        left: 0,
        top: 0,
        width,
        height,
        pixel_width: width,
        pixel_height: height,
    };
    let screen = pb::Screen {
        id: 0,
        fps,
        resolutions: vec![rect],
        current_resolution: Some(rect),
        screen_type: pb::ScreenType::Physics as i32,
        init_resolution: Some(rect),
        is_primary_screen: true,
        dpr: 1.0,
        dpi_scale: None,
        display_name: String::new(),
        resolution_type: pb::ChooseResolutionType::ChooseTypeUnknown as i32,
        video_track_index: 0,
        builtin_screen_type: pb::BuiltinScreenType::BsUnkown as i32,
    };

    let mut encoder_cap_list = vec![pb::EncoderCap {
        capture_mode: pb::CaptureMode::Desktop as i32,
        fps,
        corec_type: pb::CodecType::H264 as i32,
        resolution_width: width,
        resolution_height: height,
    }];
    if support_hevc {
        encoder_cap_list.push(pb::EncoderCap {
            capture_mode: pb::CaptureMode::Desktop as i32,
            fps,
            corec_type: pb::CodecType::H265 as i32,
            resolution_width: width,
            resolution_height: height,
        });
    }

    let messages = [
        ControlMessage::ScreenSources {
            seq: alloc_seq(),
            timestamp: timestamp_ms,
            sources: pb::ScreenSources {
                screens: vec![screen],
                current_screen_id: 0,
            },
        },
        ControlMessage::CaptureChange {
            seq: alloc_seq(),
            timestamp: timestamp_ms,
            change: pb::CaptureChange {
                capture_type: pb::capture_change::TypeEnum::CtDesktop as i32,
                type_value: 0,
                desc: String::new(),
            },
        },
        ControlMessage::CodecNegotiation {
            seq: alloc_seq(),
            timestamp: timestamp_ms,
            negotiation: pb::CodecNegotiation { encoder_cap_list },
        },
        ControlMessage::SystemMetrics {
            seq: alloc_seq(),
            timestamp: timestamp_ms,
            metrics: pb::SystemMetrics {
                metrics: vec![
                    pb::system_metrics::Metric::General as i32,
                    pb::system_metrics::Metric::Setting as i32,
                ],
                response: String::new(),
                general: Some(pb::GeneralMetric {
                    support_touch: false,
                    system_sound_support: pb::SystemSoundSupport::SsNonsupport as i32,
                    win_os_ver: 0,
                }),
                setting: Some(pb::SettingMetric {
                    keep_mute: false,
                    mute_permanent: false,
                    auto_lock: false,
                    private_screen: false,
                    file_transfer_save_location: String::new(),
                    virtual_mouse_switch: false,
                    private_screen_type: pb::PrivateScreenType::PsUnspecified as i32,
                    capture_permission_state: pb::PermissionAuthState::PermissionStateAllow as i32,
                    operation_permission_state: pb::PermissionAuthState::PermissionStateAllow
                        as i32,
                    control_allowed_state: pb::PermissionAuthState::PermissionStateAllow as i32,
                }),
            },
        },
        ControlMessage::RemoteDownloadPath {
            seq: alloc_seq(),
            timestamp: timestamp_ms,
            path: String::new(),
        },
    ];

    messages.iter().map(encode_message).collect()
}

/// Builds the `SendVideoTrackRequest` RPC sent by a desktop viewer right after `CONTROL` opens.
pub fn build_send_video_track_request(seq: i64, timestamp_ms: i64) -> Vec<u8> {
    encode_message(&ControlMessage::RpcRequest {
        seq,
        timestamp: timestamp_ms,
        request: pb::RpcRequest {
            request_header: Some(pb::RequestHeader { request_id: seq }),
            which: Some(rpc_request::Which::SendVideoTrackReq(
                pb::SendVideoTrackRequest {
                    video_track_index: Vec::new(),
                },
            )),
        },
    })
}

fn contains_annex_b_keyframe(codec: VideoCodec, bytes: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 < bytes.len() {
        let prefix = if i + 4 <= bytes.len() && bytes[i..i + 4] == [0, 0, 0, 1] {
            4
        } else if bytes[i..i + 3] == [0, 0, 1] {
            3
        } else {
            i += 1;
            continue;
        };
        let header_idx = i + prefix;
        if header_idx < bytes.len() {
            match codec {
                VideoCodec::H264 => {
                    if (bytes[header_idx] & 0x1F) == 5 {
                        return true;
                    }
                }
                VideoCodec::H265 => {
                    let nal_type = (bytes[header_idx] >> 1) & 0x3F;
                    if (16..=21).contains(&nal_type) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        i += prefix;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::inspect_protobuf_shape;

    #[test]
    fn protobuf_shape_skips_payloads_without_exposing_nested_tags() {
        let bytes = [
            0x08, 0x96, 0x01, // field 1, varint
            0x11, 0, 1, 2, 3, 4, 5, 6, 7, // field 2, fixed64
            0x1A, 0x02, 0x08, 0x01, // field 3, length-delimited (nested field)
            0xAD, 0x02, 0, 1, 2, 3, // field 37, fixed32
        ];
        assert_eq!(
            inspect_protobuf_shape(&bytes),
            [(1, 0), (2, 1), (3, 2), (37, 5)]
        );
    }

    #[test]
    fn protobuf_shape_stops_at_truncation() {
        for incomplete in [&[0x80][..], &[0x10, 0x80], &[0x12, 2, 0], &[0x15, 0, 0]] {
            let mut bytes = vec![0x08, 0x01];
            bytes.extend_from_slice(incomplete);
            assert_eq!(inspect_protobuf_shape(&bytes), [(1, 0)]);
        }
    }

    #[test]
    fn protobuf_shape_stops_at_invalid_keys_and_unsupported_wire_types() {
        for key in [0x00, 0x13, 0x14, 0x16, 0x17] {
            assert_eq!(inspect_protobuf_shape(&[0x08, 0, key, 0x18, 0]), [(1, 0)]);
        }
        assert!(inspect_protobuf_shape(&[0x80, 0x80, 0x80, 0x80, 0x10, 0]).is_empty());
    }

    #[test]
    fn protobuf_shape_caps_the_number_of_fields() {
        let bytes = [0x08, 0x01].repeat(33);
        assert_eq!(inspect_protobuf_shape(&bytes), vec![(1, 0); 32]);
    }
}
