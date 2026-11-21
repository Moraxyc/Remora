//! `gv_pb.Message` control-plane envelope, echo/FeatureFlag handshake, and RPC dispatch.

use std::fmt;

use prost::Message as _;
use serde_json::{Value, json};

use crate::pb::{self, message, simple_action};
use crate::profile::FeatureCaps;

/// Decoded `gv_pb.Message` envelope on `CONTROL_DATA_CHANNEL` / `TEXT_DATA_CHANNEL`.
///
/// Unknown protobuf tags or unrecognized `which` oneof variants decode into
/// [`ControlMessage::Unknown`] without failing.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum ControlMessage {
    EchoRequest {
        seq: i64,
        timestamp: i64,
        feature_flag: Option<pb::FeatureFlag>,
        args: String,
    },
    EchoResponse {
        seq: i64,
        timestamp: i64,
        request_seq: Option<i64>,
        feature_flag: Option<pb::FeatureFlag>,
        args: String,
    },
    SimpleAction {
        seq: i64,
        timestamp: i64,
        action: pb::SimpleAction,
    },
    ScreenSources {
        seq: i64,
        timestamp: i64,
        sources: pb::ScreenSources,
    },
    CaptureChange {
        seq: i64,
        timestamp: i64,
        change: pb::CaptureChange,
    },
    CaptureConfig {
        seq: i64,
        timestamp: i64,
        config: pb::CaptureConfig,
    },
    CaptureConfigResponse {
        seq: i64,
        timestamp: i64,
        response: pb::CaptureConfigResponse,
    },
    CodecNegotiation {
        seq: i64,
        timestamp: i64,
        negotiation: pb::CodecNegotiation,
    },
    CaptureSettingSync {
        seq: i64,
        timestamp: i64,
        sync: pb::CaptureSettingSync,
    },
    SystemMetrics {
        seq: i64,
        timestamp: i64,
        metrics: pb::SystemMetrics,
    },
    SystemStateChange {
        seq: i64,
        timestamp: i64,
        change: pb::SystemStateChange,
    },
    QuerySystemState {
        seq: i64,
        timestamp: i64,
        query: pb::QuerySystemState,
    },
    ReportError {
        seq: i64,
        timestamp: i64,
        error: pb::ReportError,
    },
    ReportQosStats {
        seq: i64,
        timestamp: i64,
        stats: pb::ReportQosStats,
    },
    RemoteDownloadPath {
        seq: i64,
        timestamp: i64,
        path: String,
    },
    InputEvent {
        seq: i64,
        timestamp: i64,
        event: pb::InputEvent,
    },
    ClipboardChange {
        seq: i64,
        timestamp: i64,
        change: pb::ClipboardChange,
    },
    RpcRequest {
        seq: i64,
        timestamp: i64,
        request: pb::RpcRequest,
    },
    RpcResponse {
        seq: i64,
        timestamp: i64,
        response: pb::RpcResponse,
    },
    Other {
        seq: i64,
        timestamp: i64,
        message: Box<pb::Message>,
    },
    Unknown {
        name: String,
        raw: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlDecodeError {
    InvalidProtobuf,
}

impl fmt::Display for ControlDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProtobuf => f.write_str("control message is not valid protobuf"),
        }
    }
}

impl std::error::Error for ControlDecodeError {}

/// Decodes a `gv_pb.Message` frame.
///
/// Unknown protobuf fields and unpopulated/unrecognized `which` variants yield
/// [`ControlMessage::Unknown`] instead of returning an error.
pub fn decode_message(bytes: &[u8]) -> Result<ControlMessage, ControlDecodeError> {
    let msg = pb::Message::decode(bytes).map_err(|_| ControlDecodeError::InvalidProtobuf)?;
    let seq = msg.seq;
    let timestamp = msg.timestamp;
    let Some(which) = msg.which.clone() else {
        return Ok(ControlMessage::Unknown {
            name: "unknown".into(),
            raw: bytes.to_vec(),
        });
    };

    Ok(match which {
        message::Which::SimpleAction(action) => {
            if action.action == simple_action::ActionType::EchoRequest as i32 {
                let feature_flag = match action.params {
                    Some(simple_action::Params::FeatureFlag(ff)) => Some(ff),
                    _ => None,
                };
                ControlMessage::EchoRequest {
                    seq,
                    timestamp,
                    feature_flag,
                    args: action.args,
                }
            } else if action.action == simple_action::ActionType::EchoResponse as i32 {
                let request_seq = serde_json::from_str::<Value>(&action.args)
                    .ok()
                    .and_then(|v| v.get("seq").and_then(Value::as_i64));
                let feature_flag = match action.params {
                    Some(simple_action::Params::FeatureFlag(ff)) => Some(ff),
                    _ => None,
                };
                ControlMessage::EchoResponse {
                    seq,
                    timestamp,
                    request_seq,
                    feature_flag,
                    args: action.args,
                }
            } else {
                ControlMessage::SimpleAction {
                    seq,
                    timestamp,
                    action,
                }
            }
        }
        message::Which::Screens(sources) => ControlMessage::ScreenSources {
            seq,
            timestamp,
            sources,
        },
        message::Which::CaptureChange(change) => ControlMessage::CaptureChange {
            seq,
            timestamp,
            change,
        },
        message::Which::CaptureConfig(config) => ControlMessage::CaptureConfig {
            seq,
            timestamp,
            config,
        },
        message::Which::CaptureConfigResponse(response) => ControlMessage::CaptureConfigResponse {
            seq,
            timestamp,
            response,
        },
        message::Which::CodecNegotiation(negotiation) => ControlMessage::CodecNegotiation {
            seq,
            timestamp,
            negotiation,
        },
        message::Which::CaptureSettingSync(sync) => ControlMessage::CaptureSettingSync {
            seq,
            timestamp,
            sync,
        },
        message::Which::SystemMetrics(metrics) => ControlMessage::SystemMetrics {
            seq,
            timestamp,
            metrics,
        },
        message::Which::SystemStateChange(change) => ControlMessage::SystemStateChange {
            seq,
            timestamp,
            change,
        },
        message::Which::QuerySystemState(query) => ControlMessage::QuerySystemState {
            seq,
            timestamp,
            query,
        },
        message::Which::ReportError(error) => ControlMessage::ReportError {
            seq,
            timestamp,
            error,
        },
        message::Which::ReportQosStats(stats) => ControlMessage::ReportQosStats {
            seq,
            timestamp,
            stats,
        },
        message::Which::RemoteDownloadPath(path) => ControlMessage::RemoteDownloadPath {
            seq,
            timestamp,
            path: path.download_path,
        },
        message::Which::InputEvent(event) => ControlMessage::InputEvent {
            seq,
            timestamp,
            event,
        },
        message::Which::ClipboardChange(change) => ControlMessage::ClipboardChange {
            seq,
            timestamp,
            change,
        },
        message::Which::RpcRequest(request) => ControlMessage::RpcRequest {
            seq,
            timestamp,
            request,
        },
        message::Which::RpcResponse(response) => ControlMessage::RpcResponse {
            seq,
            timestamp,
            response,
        },
        _ => ControlMessage::Other {
            seq,
            timestamp,
            message: Box::new(msg),
        },
    })
}

/// Encodes a [`ControlMessage`] into protobuf wire bytes.
pub fn encode_message(msg: &ControlMessage) -> Vec<u8> {
    if let ControlMessage::Unknown { raw, .. } = msg {
        return raw.clone();
    }
    let pb_msg = match msg {
        ControlMessage::EchoRequest {
            seq,
            timestamp,
            feature_flag,
            args,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::SimpleAction(pb::SimpleAction {
                action: simple_action::ActionType::EchoRequest as i32,
                args: args.clone(),
                params: feature_flag.map(simple_action::Params::FeatureFlag),
            })),
        },
        ControlMessage::EchoResponse {
            seq,
            timestamp,
            request_seq,
            feature_flag,
            args,
        } => {
            let args_str = if args.is_empty() {
                request_seq
                    .map(|id| json!({ "seq": id }).to_string())
                    .unwrap_or_default()
            } else {
                args.clone()
            };
            pb::Message {
                seq: *seq,
                timestamp: *timestamp,
                which: Some(message::Which::SimpleAction(pb::SimpleAction {
                    action: simple_action::ActionType::EchoResponse as i32,
                    args: args_str,
                    params: feature_flag.map(simple_action::Params::FeatureFlag),
                })),
            }
        }
        ControlMessage::SimpleAction {
            seq,
            timestamp,
            action,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::SimpleAction(action.clone())),
        },
        ControlMessage::ScreenSources {
            seq,
            timestamp,
            sources,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::Screens(sources.clone())),
        },
        ControlMessage::CaptureChange {
            seq,
            timestamp,
            change,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::CaptureChange(change.clone())),
        },
        ControlMessage::CaptureConfig {
            seq,
            timestamp,
            config,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::CaptureConfig(*config)),
        },
        ControlMessage::CaptureConfigResponse {
            seq,
            timestamp,
            response,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::CaptureConfigResponse(response.clone())),
        },
        ControlMessage::CodecNegotiation {
            seq,
            timestamp,
            negotiation,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::CodecNegotiation(negotiation.clone())),
        },
        ControlMessage::CaptureSettingSync {
            seq,
            timestamp,
            sync,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::CaptureSettingSync(*sync)),
        },
        ControlMessage::SystemMetrics {
            seq,
            timestamp,
            metrics,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::SystemMetrics(metrics.clone())),
        },
        ControlMessage::SystemStateChange {
            seq,
            timestamp,
            change,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::SystemStateChange(change.clone())),
        },
        ControlMessage::QuerySystemState {
            seq,
            timestamp,
            query,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::QuerySystemState(*query)),
        },
        ControlMessage::ReportError {
            seq,
            timestamp,
            error,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::ReportError(error.clone())),
        },
        ControlMessage::ReportQosStats {
            seq,
            timestamp,
            stats,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::ReportQosStats(stats.clone())),
        },
        ControlMessage::RemoteDownloadPath {
            seq,
            timestamp,
            path,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::RemoteDownloadPath(pb::RemoteDownloadPath {
                download_path: path.clone(),
            })),
        },
        ControlMessage::InputEvent {
            seq,
            timestamp,
            event,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::InputEvent(event.clone())),
        },
        ControlMessage::ClipboardChange {
            seq,
            timestamp,
            change,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::ClipboardChange(change.clone())),
        },
        ControlMessage::RpcRequest {
            seq,
            timestamp,
            request,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::RpcRequest(request.clone())),
        },
        ControlMessage::RpcResponse {
            seq,
            timestamp,
            response,
        } => pb::Message {
            seq: *seq,
            timestamp: *timestamp,
            which: Some(message::Which::RpcResponse(response.clone())),
        },
        ControlMessage::Other { message, .. } => (**message).clone(),
        ControlMessage::Unknown { .. } => unreachable!(),
    };
    pb_msg.encode_to_vec()
}

/// Builds an echo request envelope (`SimpleAction` action 0).
pub fn echo_request(
    seq: i64,
    timestamp: i64,
    feature_flag: Option<pb::FeatureFlag>,
) -> ControlMessage {
    ControlMessage::EchoRequest {
        seq,
        timestamp,
        feature_flag,
        args: String::new(),
    }
}

/// Builds an echo response envelope (`SimpleAction` action 1) carrying `{"seq": request_seq}`.
pub fn echo_response(
    seq: i64,
    timestamp: i64,
    request_seq: i64,
    feature_flag: Option<pb::FeatureFlag>,
) -> ControlMessage {
    ControlMessage::EchoResponse {
        seq,
        timestamp,
        request_seq: Some(request_seq),
        feature_flag,
        args: json!({ "seq": request_seq }).to_string(),
    }
}

/// Converts a client profile's [`FeatureCaps`] into a protobuf [`pb::FeatureFlag`].
pub const fn feature_flag_from_caps(caps: &FeatureCaps) -> pb::FeatureFlag {
    pb::FeatureFlag {
        ff_capture_setting: caps.capture_setting,
        ff_simple_action: caps.simple_action,
        ff_system_metrics: caps.system_metrics,
        ff_private_screen: caps.private_screen,
        ff_update_acquire: caps.update_acquire,
        ff_file_transfer_ftp: caps.file_transfer_ftp,
        ff_file_transfer_ftp2: caps.file_transfer_ftp2,
        ff_clipboard: caps.clipboard,
        ff_qos_stat: caps.qos_stat,
        ff_mumu_control: caps.mumu_control,
        ff_virtual_mouse_device: caps.virtual_mouse_device,
    }
}

impl fmt::Debug for ControlMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EchoRequest {
                seq,
                timestamp,
                feature_flag,
                ..
            } => f
                .debug_struct("EchoRequest")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("has_feature_flag", &feature_flag.is_some())
                .finish(),
            Self::EchoResponse {
                seq,
                timestamp,
                request_seq,
                feature_flag,
                ..
            } => f
                .debug_struct("EchoResponse")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("request_seq", request_seq)
                .field("has_feature_flag", &feature_flag.is_some())
                .finish(),
            Self::SimpleAction {
                seq,
                timestamp,
                action,
            } => f
                .debug_struct("SimpleAction")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("action", &action.action)
                .finish(),
            Self::ScreenSources {
                seq,
                timestamp,
                sources,
            } => f
                .debug_struct("ScreenSources")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("screens", &sources.screens.len())
                .finish(),
            Self::CaptureChange {
                seq,
                timestamp,
                change,
            } => f
                .debug_struct("CaptureChange")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("capture_type", &change.capture_type)
                .finish(),
            Self::CaptureConfig { seq, timestamp, .. } => f
                .debug_struct("CaptureConfig")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .finish(),
            Self::CaptureConfigResponse { seq, timestamp, .. } => f
                .debug_struct("CaptureConfigResponse")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .finish(),
            Self::CodecNegotiation {
                seq,
                timestamp,
                negotiation,
            } => f
                .debug_struct("CodecNegotiation")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("caps", &negotiation.encoder_cap_list.len())
                .finish(),
            Self::CaptureSettingSync { seq, timestamp, .. } => f
                .debug_struct("CaptureSettingSync")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .finish(),
            Self::SystemMetrics { seq, timestamp, .. } => f
                .debug_struct("SystemMetrics")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .finish(),
            Self::SystemStateChange { seq, timestamp, .. } => f
                .debug_struct("SystemStateChange")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .finish(),
            Self::QuerySystemState { seq, timestamp, .. } => f
                .debug_struct("QuerySystemState")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .finish(),
            Self::ReportError {
                seq,
                timestamp,
                error,
            } => f
                .debug_struct("ReportError")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("error_code", &error.error_code)
                .finish(),
            Self::ReportQosStats { seq, timestamp, .. } => f
                .debug_struct("ReportQosStats")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .finish(),
            Self::RemoteDownloadPath { seq, timestamp, .. } => f
                .debug_struct("RemoteDownloadPath")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .finish(),
            Self::InputEvent { seq, timestamp, .. } => f
                .debug_struct("InputEvent")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .finish(),
            Self::ClipboardChange {
                seq,
                timestamp,
                change,
            } => f
                .debug_struct("ClipboardChange")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field("content_id", &change.content_id)
                .field("len", &change.content.len())
                .field("to_be_continued", &change.to_be_continued)
                .finish(),
            Self::RpcRequest {
                seq,
                timestamp,
                request,
            } => f
                .debug_struct("RpcRequest")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field(
                    "request_id",
                    &request.request_header.as_ref().map(|h| h.request_id),
                )
                .finish(),
            Self::RpcResponse {
                seq,
                timestamp,
                response,
            } => f
                .debug_struct("RpcResponse")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .field(
                    "request_id",
                    &response.response_header.as_ref().map(|h| h.request_id),
                )
                .finish(),
            Self::Other { seq, timestamp, .. } => f
                .debug_struct("Other")
                .field("seq", seq)
                .field("timestamp", timestamp)
                .finish(),
            Self::Unknown { name, raw } => f
                .debug_struct("Unknown")
                .field("name", name)
                .field("len", &raw.len())
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_echo_request_and_response() {
        let ff = pb::FeatureFlag {
            ff_capture_setting: 4,
            ..Default::default()
        };
        let req = echo_request(10, 100, Some(ff));
        let bytes = encode_message(&req);
        assert_eq!(decode_message(&bytes).unwrap(), req);

        let resp = echo_response(1, 2, 200, Some(ff));
        let resp_bytes = encode_message(&resp);
        let decoded = decode_message(&resp_bytes).unwrap();
        assert_eq!(decoded, resp);
    }

    #[test]
    fn unknown_which_tag_decodes_as_unknown_without_error() {
        // Message { seq: 5, field 99 = varint 1 }
        let raw = [0x08, 0x05, 0x98, 0x06, 0x01];
        let decoded = decode_message(&raw).unwrap();
        assert_eq!(
            decoded,
            ControlMessage::Unknown {
                name: "unknown".into(),
                raw: raw.to_vec(),
            }
        );
    }
}
