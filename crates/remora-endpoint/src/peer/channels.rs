//! Bridges `str0m` WebRTC data channels and media tracks with `remora_proto::session` codecs
//! and [`crate::platform`] frame types.

use std::collections::{HashMap, VecDeque};
use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::time::{Duration, Instant};

use prost::encoding::{WireType, decode_key, decode_varint, skip_field};
use remora_proto::pb::{self, rpc_request};
use remora_proto::session::DataChannel;
use remora_proto::session::channels as official_channels;
use remora_proto::session::control::{ControlMessage, encode_message};
use remora_proto::session::media::{
    AudioCodec, VideoCodec, decode_capture_index_ext, decode_new_frame_ext,
    encode_capture_index_ext, encode_new_frame_ext, rtp_ext,
};
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaData, MediaTime, Mid, Pt};
use str0m::rtp::{Extension, ExtensionSerializer, ExtensionValues};
use str0m::{Rtc, RtcConfig};

use crate::platform::{Bytes, EncodedAudioFrame, EncodedVideoFrame};

/// Maximum ordered frames buffered per channel while closed or SCTP-backpressured.
pub const OUTBOX_CAPACITY: usize = 64;

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

    /// Returns true once CONTROL, TEXT, FILE, and BINARY data channels are all open.
    pub fn has_tool_channels(&self) -> bool {
        self.channel_id(&DataChannel::Control).is_some()
            && self.channel_id(&DataChannel::Text).is_some()
            && self.channel_id(&DataChannel::File).is_some()
            && self.channel_id(&DataChannel::Binary).is_some()
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

#[derive(Debug, Default)]
struct ChannelBuffer {
    replies: VecDeque<Vec<u8>>,
    latest_metrics: Option<Vec<u8>>,
    overflow_reported: bool,
}

/// Result of attempting to buffer an ordered outbound frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueStatus {
    Queued,
    DroppedFirstOverflow,
    Dropped,
}

/// Per-channel outbound buffer with bounded FIFO replies and a single coalescing metrics slot.
#[derive(Debug, Default)]
pub struct ChannelOutbox {
    buffers: HashMap<DataChannel, ChannelBuffer>,
}

impl ChannelOutbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_pending(&self, channel: &DataChannel) -> bool {
        self.buffers
            .get(channel)
            .is_some_and(|buf| !buf.replies.is_empty() || buf.latest_metrics.is_some())
    }

    pub fn has_any_pending(&self) -> bool {
        self.buffers
            .values()
            .any(|buf| !buf.replies.is_empty() || buf.latest_metrics.is_some())
    }

    pub fn enqueue_reply(&mut self, channel: DataChannel, data: Vec<u8>) -> EnqueueStatus {
        let buf = self.buffers.entry(channel).or_default();
        if buf.replies.len() >= OUTBOX_CAPACITY {
            if !buf.overflow_reported {
                buf.overflow_reported = true;
                return EnqueueStatus::DroppedFirstOverflow;
            }
            return EnqueueStatus::Dropped;
        }
        buf.replies.push_back(data);
        EnqueueStatus::Queued
    }

    pub fn set_metrics(&mut self, channel: DataChannel, data: Vec<u8>) {
        let buf = self.buffers.entry(channel).or_default();
        buf.latest_metrics = Some(data);
    }

    /// Flushes queued frames for `channel`, preserving unsent frames on `ChannelNotOpen` or `BufferFull`.
    pub fn flush_channel(
        &mut self,
        rtc: &mut Rtc,
        registry: &ChannelRegistry,
        channel: &DataChannel,
    ) -> Result<usize, ChannelError> {
        let Some(buf) = self.buffers.get_mut(channel) else {
            return Ok(0);
        };
        let mut sent = 0usize;
        while let Some(front) = buf.replies.front() {
            match write_channel(rtc, registry, channel, front) {
                Ok(()) => {
                    buf.replies.pop_front();
                    sent += 1;
                }
                Err(err) => return if sent > 0 { Ok(sent) } else { Err(err) },
            }
        }
        if let Some(metrics) = buf.latest_metrics.as_deref() {
            match write_channel(rtc, registry, channel, metrics) {
                Ok(()) => {
                    buf.latest_metrics = None;
                    sent += 1;
                }
                Err(err) => return if sent > 0 { Ok(sent) } else { Err(err) },
            }
        }
        Ok(sent)
    }

    /// Flushes every channel that is currently open in `registry` and has pending frames.
    pub fn flush_open(&mut self, rtc: &mut Rtc, registry: &ChannelRegistry) -> usize {
        let channels: Vec<DataChannel> = self
            .buffers
            .iter()
            .filter(|(ch, buf)| {
                registry.channel_id(ch).is_some()
                    && (!buf.replies.is_empty() || buf.latest_metrics.is_some())
            })
            .map(|(ch, _)| ch.clone())
            .collect();
        let mut total = 0usize;
        for channel in channels {
            if let Ok(n) = self.flush_channel(rtc, registry, &channel) {
                total += n;
            }
        }
        total
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

const CAPTURE_INDEX_EXT_ID: u8 = 9;
const NEW_FRAME_EXT_ID: u8 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CaptureIndexExt(u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NewFrameExt(bool);

#[derive(Debug)]
struct CaptureIndexSerializer;

impl ExtensionSerializer for CaptureIndexSerializer {
    fn write_to(&self, buf: &mut [u8], ev: &ExtensionValues) -> usize {
        let Some(CaptureIndexExt(index)) = ev.user_values.get::<CaptureIndexExt>() else {
            return 0;
        };
        let Some(dst) = buf.get_mut(..2) else {
            return 0;
        };
        dst.copy_from_slice(&encode_capture_index_ext(*index));
        2
    }

    fn parse_value(&self, buf: &[u8], ev: &mut ExtensionValues) -> bool {
        let Some(index) = decode_capture_index_ext(buf) else {
            return false;
        };
        ev.user_values.set(CaptureIndexExt(index));
        true
    }

    fn is_video(&self) -> bool {
        true
    }

    fn is_audio(&self) -> bool {
        false
    }
}

#[derive(Debug)]
struct NewFrameSerializer;

impl ExtensionSerializer for NewFrameSerializer {
    fn write_to(&self, buf: &mut [u8], ev: &ExtensionValues) -> usize {
        let Some(NewFrameExt(is_new)) = ev.user_values.get::<NewFrameExt>() else {
            return 0;
        };
        let Some(dst) = buf.first_mut() else {
            return 0;
        };
        *dst = encode_new_frame_ext(*is_new)[0];
        1
    }

    fn parse_value(&self, buf: &[u8], ev: &mut ExtensionValues) -> bool {
        let Some(is_new) = decode_new_frame_ext(buf) else {
            return false;
        };
        ev.user_values.set(NewFrameExt(is_new));
        true
    }

    fn is_video(&self) -> bool {
        true
    }

    fn is_audio(&self) -> bool {
        false
    }
}

/// Registers the UU `video-capture-index` and `video-frame-is-new-frame` RTP header extensions.
pub fn register_video_extensions(config: RtcConfig) -> RtcConfig {
    config
        .set_extension(
            CAPTURE_INDEX_EXT_ID,
            Extension::with_serializer(rtp_ext::CAPTURE_INDEX_URI, CaptureIndexSerializer),
        )
        .set_extension(
            NEW_FRAME_EXT_ID,
            Extension::with_serializer(rtp_ext::NEW_FRAME_URI, NewFrameSerializer),
        )
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
    let mut writer = rtc.writer(mid).ok_or(ChannelError::NoVideoTrack)?;
    let pt = select_video_pt(writer.payload_params(), target_codec)
        .ok_or_else(|| ChannelError::UnsupportedCodec(format!("{:?}", frame.codec)))?;
    if let Some(index) = frame.capture_index {
        writer = writer.user_extension_value(CaptureIndexExt(index));
    }
    if let Some(is_new) = frame.is_new_frame {
        writer = writer.user_extension_value(NewFrameExt(is_new));
    }
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
    let capture_index = data
        .ext_vals
        .user_values
        .get::<CaptureIndexExt>()
        .map(|ext| ext.0);
    let is_new_frame = data
        .ext_vals
        .user_values
        .get::<NewFrameExt>()
        .map(|ext| ext.0);
    Some(EncodedVideoFrame {
        codec,
        is_keyframe,
        pts_ms,
        capture_index,
        is_new_frame,
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

fn is_complete_wire_message(mut bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let mut count = 0usize;
    while !bytes.is_empty() {
        let Ok((number, wire_type)) = decode_key(&mut bytes) else {
            return false;
        };
        if number == 0
            || matches!(wire_type, WireType::StartGroup | WireType::EndGroup)
            || skip_field(wire_type, number, &mut bytes, Default::default()).is_err()
        {
            return false;
        }
        count += 1;
    }
    count > 0
}

/// Formats a content-free structural summary of a protobuf wire payload (up to 3 nested levels,
/// 16 fields per level): field numbers, varints, and byte lengths only, never raw byte contents.
pub fn format_protobuf_shape(bytes: &[u8]) -> String {
    fn walk(mut bytes: &[u8], depth: u8, out: &mut String) {
        const MAX_DEPTH: u8 = 3;
        const MAX_FIELDS: usize = 16;
        let mut count = 0usize;
        while !bytes.is_empty() {
            if count >= MAX_FIELDS {
                out.push_str(" ...");
                return;
            }
            let Ok((number, wire_type)) = decode_key(&mut bytes) else {
                if count > 0 {
                    out.push(' ');
                }
                out.push('?');
                return;
            };
            if count > 0 {
                out.push(' ');
            }
            match wire_type {
                WireType::Varint => {
                    let Ok(v) = decode_varint(&mut bytes) else {
                        out.push('?');
                        return;
                    };
                    let _ = write!(out, "{number}=v{v}");
                }
                WireType::SixtyFourBit => {
                    if bytes.len() < 8 {
                        out.push('?');
                        return;
                    }
                    bytes = &bytes[8..];
                    let _ = write!(out, "{number}:f64");
                }
                WireType::ThirtyTwoBit => {
                    if bytes.len() < 4 {
                        out.push('?');
                        return;
                    }
                    bytes = &bytes[4..];
                    let _ = write!(out, "{number}:f32");
                }
                WireType::LengthDelimited => {
                    let Ok(len) = decode_varint(&mut bytes) else {
                        out.push('?');
                        return;
                    };
                    let Ok(len) = usize::try_from(len) else {
                        out.push('?');
                        return;
                    };
                    let Some((payload, rest)) = bytes.split_at_checked(len) else {
                        out.push('?');
                        return;
                    };
                    bytes = rest;
                    let _ = write!(out, "{number}:{}b", payload.len());
                    if depth < MAX_DEPTH && is_complete_wire_message(payload) {
                        out.push('{');
                        walk(payload, depth + 1, out);
                        out.push('}');
                    }
                }
                WireType::StartGroup | WireType::EndGroup => {
                    let _ = write!(out, "{number}:group?");
                    return;
                }
            }
            count += 1;
        }
    }

    let mut out = String::new();
    walk(bytes, 0, &mut out);
    out
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
    use prost::Message as _;
    use remora_proto::pb;
    use remora_proto::session::DataChannel;

    use super::{
        ChannelOutbox, EnqueueStatus, OUTBOX_CAPACITY, format_protobuf_shape,
        inspect_protobuf_shape,
    };

    #[test]
    fn format_protobuf_shape_summarizes_structure_without_leaking_bytes() {
        let rpc = pb::RpcRequest {
            request_header: Some(pb::RequestHeader { request_id: 42 }),
            which: Some(pb::rpc_request::Which::TextChangeRequest(
                pb::ClipboardTextChangeRequest {
                    format_id: 1,
                    data: "secret-clip-text".to_owned(),
                },
            )),
        };
        let encoded = rpc.encode_to_vec();
        let summary = format_protobuf_shape(&encoded);
        assert!(summary.contains("1=v42"), "{summary}");
        assert!(summary.contains("1=v1"), "{summary}");
        assert!(summary.contains("2:16b"), "{summary}");
        assert!(!summary.contains("secret"), "{summary}");
    }

    #[test]
    fn format_protobuf_shape_bounds_depth_fields_and_truncation() {
        let wide = [0x08, 0x01].repeat(18);
        let summary = format_protobuf_shape(&wide);
        assert!(summary.ends_with(" ..."), "{summary}");

        let truncated = [0x08, 0x01, 0x12, 0x05, 0x01];
        assert_eq!(format_protobuf_shape(&truncated), "1=v1 ?");
    }

    #[test]
    fn outbox_coalesces_metrics_and_bounds_fifo_replies() {
        let mut outbox = ChannelOutbox::new();
        for i in 0..1_000u32 {
            outbox.set_metrics(DataChannel::Text, i.to_le_bytes().to_vec());
        }
        for i in 0..OUTBOX_CAPACITY {
            assert_eq!(
                outbox.enqueue_reply(DataChannel::Text, vec![i as u8]),
                EnqueueStatus::Queued
            );
        }
        assert_eq!(
            outbox.enqueue_reply(DataChannel::Text, vec![255]),
            EnqueueStatus::DroppedFirstOverflow
        );
        assert_eq!(
            outbox.enqueue_reply(DataChannel::Text, vec![254]),
            EnqueueStatus::Dropped
        );
        let buf = outbox.buffers.get(&DataChannel::Text).unwrap();
        assert_eq!(buf.replies.len(), OUTBOX_CAPACITY);
        assert_eq!(buf.replies.front().unwrap(), &vec![0u8]);
        assert_eq!(
            buf.replies.back().unwrap(),
            &vec![(OUTBOX_CAPACITY - 1) as u8]
        );
        assert_eq!(
            buf.latest_metrics.as_deref(),
            Some(&999u32.to_le_bytes()[..])
        );
    }

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
