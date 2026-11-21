//! Socket.IO signaling event names, typed payloads, and tolerant codecs.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::socketio::{Event, encode_binary_event, encode_event, placeholder};

/// Event names exchanged on the signaling Socket.IO namespace.
pub mod names {
    pub const ROOM_INFO: &str = "room_info";
    pub const CONTROL: &str = "control";
    pub const BE_CONTROLLED: &str = "be-controlled";
    pub const SOAC: &str = "soac";
    pub const RELEASED: &str = "released";
    pub const LEFT: &str = "left";
    pub const LEAVE: &str = "leave";
    pub const BMSG_PUSH: &str = "bmsg_push";
    pub const FORWARD_SETTING: &str = "forward_setting";
    pub const ICE_FINISHED_LOG: &str = "ice_finished_log";
    pub const REFRESH_RECONNECT_KEY: &str = "refresh_reconnect_key";
}

/// Host readiness notification (`room_info`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoomInfoEvent {
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Reconnect-key refresh request (`refresh_reconnect_key`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RefreshReconnectKeyEvent {
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Viewer control request (`control`, carries `ConnectOptions` as binary attachment 0).
#[derive(Clone, Default, PartialEq)]
pub struct ControlEvent {
    pub app_control_id: Option<String>,
    pub streamer_data: Option<Value>,
    pub connect_options: Vec<u8>,
    pub extra: Map<String, Value>,
}

impl fmt::Debug for ControlEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlEvent")
            .field("app_control_id", &self.app_control_id)
            .field("has_streamer_data", &self.streamer_data.is_some())
            .field("connect_options_len", &self.connect_options.len())
            .finish()
    }
}

/// Cloud-to-host control assignment (`be-controlled`, carries `ConnectOptions` as binary attachment 0).
#[derive(Clone, Default, PartialEq)]
pub struct BeControlledEvent {
    pub ice_id: String,
    pub client_id: String,
    pub app_control_id: Option<String>,
    pub ice_servers: Value,
    pub force_relay: bool,
    pub streamer_data: Option<Value>,
    pub controller_platform: Option<u8>,
    pub connect_options: Vec<u8>,
    pub extra: Map<String, Value>,
}

impl fmt::Debug for BeControlledEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BeControlledEvent")
            .field("ice_id", &self.ice_id)
            .field("client_id", &self.client_id)
            .field("app_control_id", &self.app_control_id)
            .field("ice_servers", &"<redacted>")
            .field("force_relay", &self.force_relay)
            .field("controller_platform", &self.controller_platform)
            .field("connect_options_len", &self.connect_options.len())
            .finish()
    }
}

/// SDP/ICE signaling payload relayed between viewer and host by `ice_id`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SoacPayload {
    pub ice_id: String,
    #[serde(flatten)]
    pub body: Value,
}

impl fmt::Debug for SoacPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SoacPayload")
            .field("ice_id", &self.ice_id)
            .field("kind", &self.body.get("type").and_then(Value::as_str))
            .finish()
    }
}

/// Relayed `soac` offer, answer, or ICE candidate event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoacEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SoacEvent {
    /// Extracts the `ice_id` from either `data.ice_id` or top-level `extra["ice_id"]`.
    pub fn ice_id(&self) -> Option<&str> {
        self.data
            .as_ref()
            .and_then(|d| d.get("ice_id"))
            .or_else(|| self.extra.get("ice_id"))
            .and_then(Value::as_str)
    }

    /// Extracts the signaling message type (`"offer"`, `"answer"`, `"candidate"`, etc.).
    pub fn kind(&self) -> Option<&str> {
        self.data
            .as_ref()
            .and_then(|d| d.get("type"))
            .or_else(|| self.extra.get("type"))
            .and_then(Value::as_str)
    }
}

/// Relayed `forward_setting` capability / sender-parameter event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForwardSettingEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default)]
    pub data: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Host telemetry event emitted once ICE completes (`ice_finished_log`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IceFinishedLogEvent {
    pub ice_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_control_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Peer release notification (`released`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleasedEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ice_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Peer or room departure notification (`left` / `leave`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LeftEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ice_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl LeftEvent {
    /// `true` when the cloud kicked the host from the room (no `ice_id` present).
    pub fn is_room_kick(&self) -> bool {
        self.ice_id.as_deref().is_none_or(str::is_empty)
    }
}

/// Server push event (`bmsg_push`: device readiness, Wake-on-LAN, etc.).
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BmsgPushEvent {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl fmt::Debug for BmsgPushEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BmsgPushEvent")
            .field("kind", &self.kind)
            .field("has_data", &self.data.is_some())
            .finish()
    }
}

impl BmsgPushEvent {
    /// Extracts `data.publisher_availability_status` if present.
    pub fn publisher_availability_status(&self) -> Option<&str> {
        self.data
            .as_ref()?
            .get("publisher_availability_status")
            .and_then(Value::as_str)
    }
}

/// Decoded Socket.IO signaling event.
///
/// Unknown event names and unexpected payload shapes decode into [`SignalEvent::Unknown`]
/// without failing.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SignalEvent {
    RoomInfo(RoomInfoEvent),
    RefreshReconnectKey(RefreshReconnectKeyEvent),
    Control(ControlEvent),
    BeControlled(BeControlledEvent),
    Soac(SoacEvent),
    ForwardSetting(ForwardSettingEvent),
    IceFinishedLog(IceFinishedLogEvent),
    Released(ReleasedEvent),
    Left(LeftEvent),
    Leave(LeftEvent),
    BmsgPush(BmsgPushEvent),
    Unknown { name: String, raw: Value },
}

impl SignalEvent {
    /// Returns the wire event name.
    pub fn name(&self) -> &str {
        match self {
            Self::RoomInfo(_) => names::ROOM_INFO,
            Self::RefreshReconnectKey(_) => names::REFRESH_RECONNECT_KEY,
            Self::Control(_) => names::CONTROL,
            Self::BeControlled(_) => names::BE_CONTROLLED,
            Self::Soac(_) => names::SOAC,
            Self::ForwardSetting(_) => names::FORWARD_SETTING,
            Self::IceFinishedLog(_) => names::ICE_FINISHED_LOG,
            Self::Released(_) => names::RELEASED,
            Self::Left(_) => names::LEFT,
            Self::Leave(_) => names::LEAVE,
            Self::BmsgPush(_) => names::BMSG_PUSH,
            Self::Unknown { name, .. } => name,
        }
    }

    /// Decodes a Socket.IO [`Event`], never failing on unknown events or unexpected fields.
    pub fn from_event(event: &Event) -> Self {
        let first = event.args.first().cloned().unwrap_or(Value::Null);
        let raw_fallback = || match event.args.len() {
            0 => Value::Null,
            1 => first.clone(),
            _ => Value::Array(event.args.clone()),
        };

        match event.name.as_str() {
            names::ROOM_INFO => {
                let extra = first.as_object().cloned().unwrap_or_default();
                Self::RoomInfo(RoomInfoEvent { extra })
            }
            names::REFRESH_RECONNECT_KEY => {
                let extra = first.as_object().cloned().unwrap_or_default();
                Self::RefreshReconnectKey(RefreshReconnectKeyEvent { extra })
            }
            names::CONTROL => {
                let Some(mut obj) = first.as_object().cloned() else {
                    return Self::Unknown {
                        name: event.name.clone(),
                        raw: raw_fallback(),
                    };
                };
                let app_control_id = obj
                    .remove("app_control_id")
                    .and_then(|v| v.as_str().map(str::to_owned));
                let streamer_data = obj.remove("streamer_data");
                let connect_options = obj
                    .remove("app_data")
                    .and_then(|v| event.attachment(&v).map(|b| b.to_vec()))
                    .or_else(|| event.attachments.first().cloned())
                    .unwrap_or_default();
                Self::Control(ControlEvent {
                    app_control_id,
                    streamer_data,
                    connect_options,
                    extra: obj,
                })
            }
            names::BE_CONTROLLED => {
                let Some(mut obj) = first.as_object().cloned() else {
                    return Self::Unknown {
                        name: event.name.clone(),
                        raw: raw_fallback(),
                    };
                };
                let Some(ice_id) = obj
                    .remove("ice_id")
                    .and_then(|v| v.as_str().map(str::to_owned))
                else {
                    return Self::Unknown {
                        name: event.name.clone(),
                        raw: raw_fallback(),
                    };
                };
                let client_id = obj
                    .remove("client_id")
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default();
                let app_control_id = obj
                    .remove("app_control_id")
                    .and_then(|v| v.as_str().map(str::to_owned));
                let ice_servers = obj.remove("iceServers").unwrap_or_else(|| json!([]));
                let force_relay = obj
                    .remove("force_relay")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let streamer_data = obj.remove("streamer_data");
                let controller_platform = obj
                    .remove("controller_platform")
                    .and_then(|v| v.as_u64())
                    .and_then(|v| u8::try_from(v).ok());
                let connect_options = obj
                    .remove("app_data")
                    .and_then(|v| event.attachment(&v).map(|b| b.to_vec()))
                    .or_else(|| event.attachments.first().cloned())
                    .unwrap_or_default();
                Self::BeControlled(BeControlledEvent {
                    ice_id,
                    client_id,
                    app_control_id,
                    ice_servers,
                    force_relay,
                    streamer_data,
                    controller_platform,
                    connect_options,
                    extra: obj,
                })
            }
            names::SOAC => serde_json::from_value::<SoacEvent>(first.clone())
                .map(Self::Soac)
                .unwrap_or_else(|_| Self::Unknown {
                    name: event.name.clone(),
                    raw: raw_fallback(),
                }),
            names::FORWARD_SETTING => serde_json::from_value::<ForwardSettingEvent>(first.clone())
                .map(Self::ForwardSetting)
                .unwrap_or_else(|_| Self::Unknown {
                    name: event.name.clone(),
                    raw: raw_fallback(),
                }),
            names::ICE_FINISHED_LOG => serde_json::from_value::<IceFinishedLogEvent>(first.clone())
                .map(Self::IceFinishedLog)
                .unwrap_or_else(|_| Self::Unknown {
                    name: event.name.clone(),
                    raw: raw_fallback(),
                }),
            names::RELEASED => serde_json::from_value::<ReleasedEvent>(first.clone())
                .map(Self::Released)
                .unwrap_or_else(|_| Self::Unknown {
                    name: event.name.clone(),
                    raw: raw_fallback(),
                }),
            names::LEFT => {
                if first.is_null() {
                    Self::Left(LeftEvent::default())
                } else {
                    serde_json::from_value::<LeftEvent>(first.clone())
                        .map(Self::Left)
                        .unwrap_or_else(|_| Self::Unknown {
                            name: event.name.clone(),
                            raw: raw_fallback(),
                        })
                }
            }
            names::LEAVE => {
                if first.is_null() {
                    Self::Leave(LeftEvent::default())
                } else {
                    serde_json::from_value::<LeftEvent>(first.clone())
                        .map(Self::Leave)
                        .unwrap_or_else(|_| Self::Unknown {
                            name: event.name.clone(),
                            raw: raw_fallback(),
                        })
                }
            }
            names::BMSG_PUSH => {
                if first.is_null() {
                    Self::BmsgPush(BmsgPushEvent::default())
                } else {
                    serde_json::from_value::<BmsgPushEvent>(first.clone())
                        .map(Self::BmsgPush)
                        .unwrap_or_else(|_| Self::Unknown {
                            name: event.name.clone(),
                            raw: raw_fallback(),
                        })
                }
            }
            _ => Self::Unknown {
                name: event.name.clone(),
                raw: raw_fallback(),
            },
        }
    }

    /// Encodes this event into a Socket.IO text frame plus zero or more binary attachment frames.
    pub fn encode(&self, ack_id: Option<u64>) -> (String, Vec<Vec<u8>>) {
        match self {
            Self::RoomInfo(info) => {
                if info.extra.is_empty() {
                    (encode_event(names::ROOM_INFO, &[], ack_id), Vec::new())
                } else {
                    (
                        encode_event(
                            names::ROOM_INFO,
                            &[Value::Object(info.extra.clone())],
                            ack_id,
                        ),
                        Vec::new(),
                    )
                }
            }
            Self::RefreshReconnectKey(req) => {
                if req.extra.is_empty() {
                    (
                        encode_event(names::REFRESH_RECONNECT_KEY, &[], ack_id),
                        Vec::new(),
                    )
                } else {
                    (
                        encode_event(
                            names::REFRESH_RECONNECT_KEY,
                            &[Value::Object(req.extra.clone())],
                            ack_id,
                        ),
                        Vec::new(),
                    )
                }
            }
            Self::Control(ctrl) => {
                let mut map = ctrl.extra.clone();
                if let Some(id) = &ctrl.app_control_id {
                    map.insert("app_control_id".into(), Value::String(id.clone()));
                }
                if let Some(sd) = &ctrl.streamer_data {
                    map.insert("streamer_data".into(), sd.clone());
                }
                map.insert("app_data".into(), placeholder(0));
                let header = encode_binary_event(names::CONTROL, &[Value::Object(map)], 1, ack_id);
                (header, vec![ctrl.connect_options.clone()])
            }
            Self::BeControlled(bc) => {
                let mut map = bc.extra.clone();
                map.insert("ice_id".into(), Value::String(bc.ice_id.clone()));
                map.insert("client_id".into(), Value::String(bc.client_id.clone()));
                if let Some(id) = &bc.app_control_id {
                    map.insert("app_control_id".into(), Value::String(id.clone()));
                }
                map.insert("iceServers".into(), bc.ice_servers.clone());
                map.insert("force_relay".into(), Value::Bool(bc.force_relay));
                if let Some(sd) = &bc.streamer_data {
                    map.insert("streamer_data".into(), sd.clone());
                }
                if let Some(plat) = bc.controller_platform {
                    map.insert("controller_platform".into(), Value::from(plat));
                }
                map.insert("app_data".into(), placeholder(0));
                let header =
                    encode_binary_event(names::BE_CONTROLLED, &[Value::Object(map)], 1, ack_id);
                (header, vec![bc.connect_options.clone()])
            }
            Self::Soac(soac) => (
                encode_event(
                    names::SOAC,
                    &[serde_json::to_value(soac).expect("soac serializes")],
                    ack_id,
                ),
                Vec::new(),
            ),
            Self::ForwardSetting(fs) => (
                encode_event(
                    names::FORWARD_SETTING,
                    &[serde_json::to_value(fs).expect("forward_setting serializes")],
                    ack_id,
                ),
                Vec::new(),
            ),
            Self::IceFinishedLog(log) => (
                encode_event(
                    names::ICE_FINISHED_LOG,
                    &[serde_json::to_value(log).expect("ice_finished_log serializes")],
                    ack_id,
                ),
                Vec::new(),
            ),
            Self::Released(rel) => (
                encode_event(
                    names::RELEASED,
                    &[serde_json::to_value(rel).expect("released serializes")],
                    ack_id,
                ),
                Vec::new(),
            ),
            Self::Left(left) => (
                encode_event(
                    names::LEFT,
                    &[serde_json::to_value(left).expect("left serializes")],
                    ack_id,
                ),
                Vec::new(),
            ),
            Self::Leave(leave) => (
                encode_event(
                    names::LEAVE,
                    &[serde_json::to_value(leave).expect("leave serializes")],
                    ack_id,
                ),
                Vec::new(),
            ),
            Self::BmsgPush(push) => (
                encode_event(
                    names::BMSG_PUSH,
                    &[serde_json::to_value(push).expect("bmsg_push serializes")],
                    ack_id,
                ),
                Vec::new(),
            ),
            Self::Unknown { name, raw } => {
                let args: Vec<Value> = match raw {
                    Value::Null => Vec::new(),
                    Value::Array(arr) => arr.clone(),
                    other => vec![other.clone()],
                };
                (encode_event(name, &args, ack_id), Vec::new())
            }
        }
    }
}

/// Decoded acknowledgement to `refresh_reconnect_key`.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefreshReconnectKeyAck {
    Success { reconnect_key: String },
    Failure { reason: Option<String> },
    Unknown { raw: Vec<Value> },
}

impl fmt::Debug for RefreshReconnectKeyAck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success { .. } => f
                .debug_struct("Success")
                .field("reconnect_key", &"<redacted>")
                .finish(),
            Self::Failure { reason } => f.debug_struct("Failure").field("reason", reason).finish(),
            Self::Unknown { raw } => f.debug_struct("Unknown").field("len", &raw.len()).finish(),
        }
    }
}

impl RefreshReconnectKeyAck {
    pub fn decode(args: &[Value]) -> Self {
        match args.first().and_then(Value::as_str) {
            Some("success") => {
                if let Some(key) = args
                    .get(1)
                    .and_then(|v| v.get("reconnect_key"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    Self::Success {
                        reconnect_key: key.to_owned(),
                    }
                } else {
                    Self::Unknown { raw: args.to_vec() }
                }
            }
            Some("failure") => {
                let reason = args
                    .get(1)
                    .and_then(|v| v.get("reason"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Self::Failure { reason }
            }
            _ => Self::Unknown { raw: args.to_vec() },
        }
    }
}

/// Decoded acknowledgement to `control`.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum ControlAck {
    Success {
        ice_id: String,
        client_id: String,
        app_control_id: Option<String>,
        ice_servers: Value,
        force_relay: bool,
    },
    Failure {
        reason: Option<String>,
    },
    Unknown {
        raw: Vec<Value>,
    },
}

impl fmt::Debug for ControlAck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success {
                ice_id,
                client_id,
                app_control_id,
                force_relay,
                ..
            } => f
                .debug_struct("Success")
                .field("ice_id", ice_id)
                .field("client_id", client_id)
                .field("app_control_id", app_control_id)
                .field("ice_servers", &"<redacted>")
                .field("force_relay", force_relay)
                .finish(),
            Self::Failure { reason } => f.debug_struct("Failure").field("reason", reason).finish(),
            Self::Unknown { raw } => f.debug_struct("Unknown").field("len", &raw.len()).finish(),
        }
    }
}

impl ControlAck {
    pub fn decode(args: &[Value]) -> Self {
        match args.first().and_then(Value::as_str) {
            Some("success") => {
                let Some(obj) = args.get(1).and_then(Value::as_object) else {
                    return Self::Unknown { raw: args.to_vec() };
                };
                let Some(ice_id) = obj.get("ice_id").and_then(Value::as_str) else {
                    return Self::Unknown { raw: args.to_vec() };
                };
                let Some(client_id) = obj.get("client_id").and_then(Value::as_str) else {
                    return Self::Unknown { raw: args.to_vec() };
                };
                Self::Success {
                    ice_id: ice_id.to_owned(),
                    client_id: client_id.to_owned(),
                    app_control_id: obj
                        .get("app_control_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    ice_servers: obj.get("iceServers").cloned().unwrap_or_else(|| json!([])),
                    force_relay: obj
                        .get("force_relay")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }
            }
            Some("failure") => {
                let reason = args
                    .get(1)
                    .and_then(|v| v.get("reason"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Self::Failure { reason }
            }
            _ => Self::Unknown { raw: args.to_vec() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::socketio::{Decoder, Packet};

    #[test]
    fn decodes_unknown_events_and_unknown_fields_without_error() {
        let mut decoder = Decoder::new();
        let Some(Packet::Event(event)) = decoder
            .text(r#"42["future_event",{"foo":1,"bar":"baz"}]"#)
            .unwrap()
        else {
            panic!("expected event");
        };
        let decoded = SignalEvent::from_event(&event);
        assert_eq!(
            decoded,
            SignalEvent::Unknown {
                name: "future_event".into(),
                raw: json!({"foo": 1, "bar": "baz"}),
            }
        );

        let Some(Packet::Event(rel)) = decoder
            .text(r#"42["released",{"ice_id":"i1","client_id":"c1","future_field":true}]"#)
            .unwrap()
        else {
            panic!("expected event");
        };
        let SignalEvent::Released(rel) = SignalEvent::from_event(&rel) else {
            panic!("expected released");
        };
        assert_eq!(rel.ice_id.as_deref(), Some("i1"));
        assert_eq!(rel.extra.get("future_field"), Some(&json!(true)));
    }

    #[test]
    fn roundtrips_binary_be_controlled_event() {
        let ev = SignalEvent::BeControlled(BeControlledEvent {
            ice_id: "ice-1".into(),
            client_id: "viewer-1".into(),
            app_control_id: Some("ctrl-1".into()),
            ice_servers: json!([{"urls": "stun:example.com:3478"}]),
            force_relay: false,
            streamer_data: Some(json!("{}")),
            controller_platform: Some(4),
            connect_options: vec![0x08, 0x01],
            extra: Map::new(),
        });
        let (header, attachments) = ev.encode(None);
        assert_eq!(attachments, vec![vec![0x08, 0x01]]);
        let mut decoder = Decoder::new();
        assert_eq!(decoder.text(&header).unwrap(), None);
        let Some(Packet::Event(decoded_raw)) = decoder.binary(&attachments[0]).unwrap() else {
            panic!("expected event");
        };
        let decoded = SignalEvent::from_event(&decoded_raw);
        assert_eq!(decoded, ev);
    }

    #[test]
    fn redacts_secrets_in_debug() {
        let ack = RefreshReconnectKeyAck::Success {
            reconnect_key: "secret-key-123".into(),
        };
        assert!(!format!("{ack:?}").contains("secret-key-123"));

        let ctrl_ack = ControlAck::Success {
            ice_id: "ice-1".into(),
            client_id: "host-1".into(),
            app_control_id: None,
            ice_servers: json!([{"urls": "turn:t", "credential": "turn-secret"}]),
            force_relay: true,
        };
        assert!(!format!("{ctrl_ack:?}").contains("turn-secret"));
    }
}
