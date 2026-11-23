//! Port-mapping frames carried on the file data channel.

use std::fmt;

use prost::Message;
use serde::{Deserialize, Serialize};

use super::{DataChannel, control::ControlMessage};
use crate::pb::{self, message, port_mapping_frame::FrameType};

pub const TUNNEL_CHANNEL: DataChannel = DataChannel::File;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TunnelFrameType {
    Syn,
    Fin,
    Data,
    SynAck,
    DataAck,
    Unknown(i32),
}

impl TunnelFrameType {
    pub fn from_wire(value: i32) -> Self {
        match FrameType::try_from(value) {
            Ok(FrameType::Syn) => Self::Syn,
            Ok(FrameType::Fin) => Self::Fin,
            Ok(FrameType::Data) => Self::Data,
            Ok(FrameType::SynAck) => Self::SynAck,
            Ok(FrameType::DataAck) => Self::DataAck,
            Err(_) => Self::Unknown(value),
        }
    }

    pub const fn to_wire(self) -> i32 {
        match self {
            Self::Syn => FrameType::Syn as i32,
            Self::Fin => FrameType::Fin as i32,
            Self::Data => FrameType::Data as i32,
            Self::SynAck => FrameType::SynAck as i32,
            Self::DataAck => FrameType::DataAck as i32,
            Self::Unknown(value) => value,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSyn {
    pub version: u32,
    pub target_host: String,
    pub target_port: u16,
}

impl fmt::Debug for TunnelSyn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelSyn")
            .field("version", &self.version)
            .field("target_host_len", &self.target_host.len())
            .field("target_port", &self.target_port)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSynAck {
    pub version: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TunnelError {
    InvalidJson,
}

impl fmt::Display for TunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => f.write_str("invalid tunnel JSON"),
        }
    }
}

impl std::error::Error for TunnelError {}

#[derive(Clone, PartialEq, Eq)]
pub struct TunnelFrame {
    pub seq: i64,
    pub timestamp: i64,
    pub session_id: u64,
    pub rule_id: u64,
    pub stream_id: u64,
    pub frame_type: TunnelFrameType,
    pub payload: Vec<u8>,
}

impl fmt::Debug for TunnelFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelFrame")
            .field("seq", &self.seq)
            .field("timestamp", &self.timestamp)
            .field("session_id", &self.session_id)
            .field("rule_id", &self.rule_id)
            .field("stream_id", &self.stream_id)
            .field("frame_type", &self.frame_type)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

pub fn extract_tunnel_frame(msg: &ControlMessage) -> Option<TunnelFrame> {
    let ControlMessage::Other {
        seq,
        timestamp,
        message,
    } = msg
    else {
        return None;
    };
    let Some(message::Which::PortMappingFrame(frame)) = &message.which else {
        return None;
    };
    Some(TunnelFrame {
        seq: *seq,
        timestamp: *timestamp,
        session_id: frame.session_id,
        rule_id: frame.rule_id,
        stream_id: frame.stream_id,
        frame_type: TunnelFrameType::from_wire(frame.r#type),
        payload: frame.payload.clone(),
    })
}

pub fn encode_tunnel_frame(frame: &TunnelFrame) -> Vec<u8> {
    encode_payload(
        frame.seq,
        frame.timestamp,
        frame.session_id,
        frame.rule_id,
        frame.stream_id,
        frame.frame_type,
        frame.payload.clone(),
    )
}

pub fn decode_syn(payload: &[u8]) -> Result<TunnelSyn, TunnelError> {
    serde_json::from_slice(payload).map_err(|_| TunnelError::InvalidJson)
}

pub fn encode_syn(
    seq: i64,
    timestamp: i64,
    session_id: u64,
    rule_id: u64,
    stream_id: u64,
    syn: &TunnelSyn,
) -> Vec<u8> {
    encode_payload(
        seq,
        timestamp,
        session_id,
        rule_id,
        stream_id,
        TunnelFrameType::Syn,
        serde_json::to_vec(syn).expect("TunnelSyn fields are JSON serializable"),
    )
}

pub fn decode_syn_ack(payload: &[u8]) -> Result<TunnelSynAck, TunnelError> {
    serde_json::from_slice(payload).map_err(|_| TunnelError::InvalidJson)
}

pub fn encode_syn_ack(
    seq: i64,
    timestamp: i64,
    session_id: u64,
    rule_id: u64,
    stream_id: u64,
    ack: &TunnelSynAck,
) -> Vec<u8> {
    encode_payload(
        seq,
        timestamp,
        session_id,
        rule_id,
        stream_id,
        TunnelFrameType::SynAck,
        serde_json::to_vec(ack).expect("TunnelSynAck fields are JSON serializable"),
    )
}

pub fn encode_data(
    seq: i64,
    timestamp: i64,
    session_id: u64,
    rule_id: u64,
    stream_id: u64,
    payload: Vec<u8>,
) -> Vec<u8> {
    encode_payload(
        seq,
        timestamp,
        session_id,
        rule_id,
        stream_id,
        TunnelFrameType::Data,
        payload,
    )
}

pub fn encode_fin(
    seq: i64,
    timestamp: i64,
    session_id: u64,
    rule_id: u64,
    stream_id: u64,
) -> Vec<u8> {
    encode_payload(
        seq,
        timestamp,
        session_id,
        rule_id,
        stream_id,
        TunnelFrameType::Fin,
        Vec::new(),
    )
}

fn encode_payload(
    seq: i64,
    timestamp: i64,
    session_id: u64,
    rule_id: u64,
    stream_id: u64,
    frame_type: TunnelFrameType,
    payload: Vec<u8>,
) -> Vec<u8> {
    pb::Message {
        seq,
        timestamp,
        which: Some(message::Which::PortMappingFrame(pb::PortMappingFrame {
            session_id,
            rule_id,
            stream_id,
            r#type: frame_type.to_wire(),
            payload,
        })),
    }
    .encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::control::decode_message;

    #[test]
    fn frame_types_match_wire_values_and_preserve_unknown_values() {
        for (wire, frame_type) in [
            (0, TunnelFrameType::Syn),
            (1, TunnelFrameType::Fin),
            (2, TunnelFrameType::Data),
            (3, TunnelFrameType::SynAck),
            (4, TunnelFrameType::DataAck),
            (73, TunnelFrameType::Unknown(73)),
            (-1, TunnelFrameType::Unknown(-1)),
        ] {
            assert_eq!(TunnelFrameType::from_wire(wire), frame_type);
            assert_eq!(frame_type.to_wire(), wire);
        }
        assert_eq!(TUNNEL_CHANNEL.as_str(), "FILE_DATA_CHANNEL");
    }

    #[test]
    fn syn_roundtrips_expected_json_and_redacts_host() {
        let syn = TunnelSyn {
            version: 1,
            target_host: "private.example".into(),
            target_port: 8080,
        };
        let control = decode_message(&encode_syn(1, 100, 10, 20, 30, &syn)).unwrap();
        let frame = extract_tunnel_frame(&control).unwrap();
        assert_eq!(
            frame,
            TunnelFrame {
                seq: 1,
                timestamp: 100,
                session_id: 10,
                rule_id: 20,
                stream_id: 30,
                frame_type: TunnelFrameType::Syn,
                payload: br#"{"version":1,"target_host":"private.example","target_port":8080}"#
                    .to_vec(),
            }
        );
        assert_eq!(decode_syn(&frame.payload), Ok(syn.clone()));
        let debug = format!("{syn:?}");
        assert!(debug.contains("target_host_len: 15"));
        assert!(debug.contains("target_port: 8080"));
        assert!(!debug.contains(&syn.target_host));
    }

    #[test]
    fn syn_ack_roundtrips_optional_error() {
        for (ok, error, payload) in [
            (true, "", br#"{"version":1,"ok":true}"#.as_slice()),
            (
                false,
                "unavailable",
                br#"{"version":1,"ok":false,"error":"unavailable"}"#.as_slice(),
            ),
        ] {
            let ack = TunnelSynAck {
                version: 1,
                ok,
                error: error.into(),
            };
            let control = decode_message(&encode_syn_ack(2, 101, 10, 20, 30, &ack)).unwrap();
            let frame = extract_tunnel_frame(&control).unwrap();
            assert_eq!(
                frame,
                TunnelFrame {
                    seq: 2,
                    timestamp: 101,
                    session_id: 10,
                    rule_id: 20,
                    stream_id: 30,
                    frame_type: TunnelFrameType::SynAck,
                    payload: payload.to_vec(),
                }
            );
            assert_eq!(decode_syn_ack(&frame.payload), Ok(ack));
        }
    }

    #[test]
    fn data_fin_and_ack_roundtrip_without_truncating_ids() {
        let (session_id, rule_id, stream_id) = (u64::MAX, 1 << 62, (1 << 32) + 1);
        let data = vec![128, 129, 130];
        let cases = [
            (
                TunnelFrameType::Data,
                data.clone(),
                encode_data(3, 102, session_id, rule_id, stream_id, data),
            ),
            (
                TunnelFrameType::Fin,
                Vec::new(),
                encode_fin(3, 102, session_id, rule_id, stream_id),
            ),
            (TunnelFrameType::DataAck, Vec::new(), Vec::new()),
            (
                TunnelFrameType::Unknown(73),
                vec![128, 129, 130],
                Vec::new(),
            ),
        ];
        for (frame_type, payload, specialized_wire) in cases {
            let expected = TunnelFrame {
                seq: 3,
                timestamp: 102,
                session_id,
                rule_id,
                stream_id,
                frame_type,
                payload,
            };
            let wire = encode_tunnel_frame(&expected);
            if matches!(frame_type, TunnelFrameType::Data | TunnelFrameType::Fin) {
                assert_eq!(wire, specialized_wire);
            }
            let control = decode_message(&wire).unwrap();
            let frame = extract_tunnel_frame(&control).unwrap();
            assert_eq!(frame, expected);
            let debug = format!("{frame:?}");
            assert!(debug.contains(&format!("payload_len: {}", frame.payload.len())));
            assert!(!debug.contains("128, 129, 130"));
            assert!(!format!("{control:?}").contains("128, 129, 130"));
        }
    }

    #[test]
    fn rejects_invalid_handshake_json_without_echoing_input() {
        assert_eq!(
            decode_syn(br#"{"version":1,"target_host":"secret","target_port":65536}"#),
            Err(TunnelError::InvalidJson)
        );
        assert_eq!(
            decode_syn_ack(br#"{"version":1,"ok":"secret"}"#),
            Err(TunnelError::InvalidJson)
        );
    }

    #[test]
    fn rejects_unrelated_control_messages() {
        let other = ControlMessage::Other {
            seq: 0,
            timestamp: 0,
            message: Box::new(pb::Message::default()),
        };
        assert_eq!(extract_tunnel_frame(&other), None);
        assert_eq!(
            extract_tunnel_frame(&crate::session::control::echo_request(1, 2, None)),
            None
        );
    }
}
