//! Binary TERM frames, terminal control RPCs, and session ownership.

use std::{collections::BTreeMap, fmt};

use prost::Message as _;
use serde::{Deserialize, Serialize};

use super::{
    DataChannel,
    control::{ControlMessage, encode_message},
};
use crate::pb::{self, message, rpc_request, rpc_response};

pub const TERMINAL_CHANNEL: DataChannel = DataChannel::Binary;
pub const TERM_MAGIC: [u8; 4] = *b"TERM";
pub const TERM_VERSION: u8 = 1;
pub const TERM_HEADER_LEN: usize = 16;
pub const MAX_TERM_PAYLOAD_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TermDecodeError {
    TooShort,
    InvalidMagic,
    UnsupportedVersion(u8),
    PayloadTooLarge(usize),
    LengthMismatch { expected: usize, actual: usize },
}

impl fmt::Display for TermDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => f.write_str("incomplete TERM header"),
            Self::InvalidMagic => f.write_str("invalid TERM magic"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported TERM version {v}"),
            Self::PayloadTooLarge(n) => write!(f, "TERM payload exceeds limit: {n} bytes"),
            Self::LengthMismatch { expected, actual } => {
                write!(f, "TERM payload length: expected {expected}, got {actual}")
            }
        }
    }
}

impl std::error::Error for TermDecodeError {}

#[derive(Clone, PartialEq, Eq)]
pub struct TermFrame {
    pub version: u8,
    pub frame_type: u8,
    pub flags: [u8; 2],
    pub session_id: u32,
    pub payload: Vec<u8>,
}

impl fmt::Debug for TermFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TermFrame")
            .field("version", &self.version)
            .field("frame_type", &self.frame_type)
            .field("flags", &self.flags)
            .field("session_id", &self.session_id)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

pub fn is_term_frame(bytes: &[u8]) -> bool {
    bytes.len() >= TERM_HEADER_LEN && bytes.starts_with(&TERM_MAGIC)
}

/// Encodes a frame. The payload length must fit the wire's 32-bit length field.
pub fn encode_term_frame(frame: &TermFrame) -> Vec<u8> {
    let len = u32::try_from(frame.payload.len()).expect("TERM length exceeds u32");
    let mut bytes = Vec::with_capacity(TERM_HEADER_LEN + frame.payload.len());
    bytes.extend_from_slice(&TERM_MAGIC);
    bytes.extend_from_slice(&[frame.version, frame.frame_type]);
    bytes.extend_from_slice(&frame.flags);
    bytes.extend_from_slice(&frame.session_id.to_le_bytes());
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&frame.payload);
    bytes
}

pub fn decode_term_frame(bytes: &[u8]) -> Result<TermFrame, TermDecodeError> {
    let (header, payload) = bytes
        .split_first_chunk::<TERM_HEADER_LEN>()
        .ok_or(TermDecodeError::TooShort)?;
    if !header.starts_with(&TERM_MAGIC) {
        return Err(TermDecodeError::InvalidMagic);
    }
    let version = header[4];
    if version != TERM_VERSION {
        return Err(TermDecodeError::UnsupportedVersion(version));
    }
    let expected = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    if expected > MAX_TERM_PAYLOAD_BYTES {
        return Err(TermDecodeError::PayloadTooLarge(expected));
    }
    if expected != payload.len() {
        return Err(TermDecodeError::LengthMismatch {
            expected,
            actual: payload.len(),
        });
    }
    Ok(TermFrame {
        version,
        frame_type: header[5],
        flags: [header[6], header[7]],
        session_id: u32::from_le_bytes(header[8..12].try_into().unwrap()),
        payload: payload.to_vec(),
    })
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermOpenOptions {
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shell: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub option: String,
}

impl fmt::Debug for TermOpenOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TermOpenOptions")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("shell_len", &self.shell.len())
            .field("option_len", &self.option.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermSessionInfo {
    pub session_id: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub shell: String,
    #[serde(default)]
    pub attached: bool,
    #[serde(default)]
    pub last_active_ms: u64,
}

impl fmt::Debug for TermSessionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TermSessionInfo")
            .field("session_id", &self.session_id)
            .field("name_len", &self.name.len())
            .field("shell_len", &self.shell.len())
            .field("attached", &self.attached)
            .finish()
    }
}

/// Resolves the session to close; the header session_id stays intact to correlate the response.
pub fn resolve_close_target_session(header_session_id: u32, payload: &[u8]) -> u32 {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| value.get("session_id")?.as_u64())
        .and_then(|id| u32::try_from(id).ok())
        .unwrap_or(header_session_id)
}

#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminalControlMessage {
    CheckEnvRequest {
        seq: i64,
        timestamp: i64,
        request_id: i64,
    },
    CheckEnvResponse {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        code: i32,
        message: String,
    },
    RenameSessionRequest {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        session_id: u32,
        name: String,
    },
    RenameSessionResponse {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        session_id: u32,
        code: i32,
        message: String,
    },
    SessionChanged {
        seq: i64,
        timestamp: i64,
    },
}

impl fmt::Debug for TerminalControlMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CheckEnvRequest { request_id, .. } => f
                .debug_struct("CheckEnvRequest")
                .field("request_id", request_id)
                .finish(),
            Self::CheckEnvResponse {
                request_id,
                code,
                message,
                ..
            } => f
                .debug_struct("CheckEnvResponse")
                .field("request_id", request_id)
                .field("code", code)
                .field("message_len", &message.len())
                .finish(),
            Self::RenameSessionRequest {
                request_id,
                session_id,
                name,
                ..
            } => f
                .debug_struct("RenameSessionRequest")
                .field("request_id", request_id)
                .field("session_id", session_id)
                .field("name_len", &name.len())
                .finish(),
            Self::RenameSessionResponse {
                request_id,
                session_id,
                code,
                message,
                ..
            } => f
                .debug_struct("RenameSessionResponse")
                .field("request_id", request_id)
                .field("session_id", session_id)
                .field("code", code)
                .field("message_len", &message.len())
                .finish(),
            Self::SessionChanged { seq, .. } => {
                f.debug_struct("SessionChanged").field("seq", seq).finish()
            }
        }
    }
}

pub fn extract_terminal_control(msg: &ControlMessage) -> Option<TerminalControlMessage> {
    use TerminalControlMessage::*;
    Some(match *msg {
        ControlMessage::RpcRequest {
            seq,
            timestamp,
            ref request,
        } => {
            let request_id = request.request_header.as_ref().map_or(0, |h| h.request_id);
            match request.which.as_ref()? {
                rpc_request::Which::TerminalCheckEnvReq(_) => CheckEnvRequest {
                    seq,
                    timestamp,
                    request_id,
                },
                rpc_request::Which::TerminalRenameSessionReq(req) => RenameSessionRequest {
                    seq,
                    timestamp,
                    request_id,
                    session_id: req.session_id,
                    name: req.name.clone(),
                },
                _ => return None,
            }
        }
        ControlMessage::RpcResponse {
            seq,
            timestamp,
            ref response,
        } => {
            let request_id = response
                .response_header
                .as_ref()
                .map_or(0, |h| h.request_id);
            match response.which.as_ref()? {
                rpc_response::Which::TerminalCheckEnvResp(resp) => CheckEnvResponse {
                    seq,
                    timestamp,
                    request_id,
                    code: resp.code,
                    message: resp.message.clone(),
                },
                rpc_response::Which::TerminalRenameSessionResp(resp) => RenameSessionResponse {
                    seq,
                    timestamp,
                    request_id,
                    session_id: resp.session_id,
                    code: resp.code,
                    message: resp.message.clone(),
                },
                _ => return None,
            }
        }
        ControlMessage::Other {
            seq,
            timestamp,
            ref message,
        } if matches!(
            message.which,
            Some(message::Which::TerminalSessionChanged(_))
        ) =>
        {
            SessionChanged { seq, timestamp }
        }
        _ => return None,
    })
}

fn encode_rpc_request(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    which: rpc_request::Which,
) -> Vec<u8> {
    encode_message(&ControlMessage::RpcRequest {
        seq,
        timestamp,
        request: pb::RpcRequest {
            request_header: Some(pb::RequestHeader { request_id }),
            which: Some(which),
        },
    })
}

fn encode_rpc_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    which: rpc_response::Which,
) -> Vec<u8> {
    encode_message(&ControlMessage::RpcResponse {
        seq,
        timestamp,
        response: pb::RpcResponse {
            response_header: Some(pb::ResponseHeader { request_id }),
            which: Some(which),
        },
    })
}

pub fn encode_check_env_request(seq: i64, timestamp: i64, request_id: i64) -> Vec<u8> {
    encode_rpc_request(
        seq,
        timestamp,
        request_id,
        rpc_request::Which::TerminalCheckEnvReq(pb::CheckEnvReq {}),
    )
}
pub fn encode_check_env_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    code: i32,
    message: &str,
) -> Vec<u8> {
    encode_rpc_response(
        seq,
        timestamp,
        request_id,
        rpc_response::Which::TerminalCheckEnvResp(pb::CheckEnvResp {
            code,
            message: message.to_owned(),
        }),
    )
}
pub fn encode_rename_session_request(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    session_id: u32,
    name: &str,
) -> Vec<u8> {
    encode_rpc_request(
        seq,
        timestamp,
        request_id,
        rpc_request::Which::TerminalRenameSessionReq(pb::RenameSessionReq {
            session_id,
            name: name.to_owned(),
        }),
    )
}
pub fn encode_rename_session_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    session_id: u32,
    code: i32,
    message: &str,
) -> Vec<u8> {
    encode_rpc_response(
        seq,
        timestamp,
        request_id,
        rpc_response::Which::TerminalRenameSessionResp(pb::RenameSessionResp {
            session_id,
            code,
            message: message.to_owned(),
        }),
    )
}
pub fn encode_session_changed(seq: i64, timestamp: i64) -> Vec<u8> {
    pb::Message {
        seq,
        timestamp,
        which: Some(message::Which::TerminalSessionChanged(
            pb::TerminalSessionChanged {},
        )),
    }
    .encode_to_vec()
}

struct Session {
    generation: u64,
    attached_peer: Option<String>,
    info: TermSessionInfo,
}

impl Session {
    fn detach(&mut self, peer_id: &str) -> bool {
        if self.attached_peer.as_deref() != Some(peer_id) {
            return false;
        }
        self.attached_peer = None;
        self.info.attached = false;
        true
    }
}

#[derive(Default)]
pub struct TerminalRegistry {
    generation: u64,
    sessions: BTreeMap<u32, Session>,
}

impl fmt::Debug for TerminalRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TerminalRegistry")
            .field("session_count", &self.sessions.len())
            .finish()
    }
}

impl TerminalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_session(
        &mut self,
        session_id: u32,
        peer_id: &str,
        name: String,
        shell: String,
        now_ms: u64,
    ) -> u64 {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("terminal generations exhausted");
        self.sessions.insert(
            session_id,
            Session {
                generation: self.generation,
                attached_peer: Some(peer_id.to_owned()),
                info: TermSessionInfo {
                    session_id,
                    name,
                    shell,
                    attached: true,
                    last_active_ms: now_ms,
                },
            },
        );
        self.generation
    }

    pub fn attach(&mut self, session_id: u32, peer_id: &str, now_ms: u64) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        session.attached_peer = Some(peer_id.to_owned());
        session.info.attached = true;
        session.info.last_active_ms = now_ms;
        true
    }

    pub fn is_owner(&self, session_id: u32, peer_id: &str) -> bool {
        self.sessions
            .get(&session_id)
            .is_some_and(|s| s.attached_peer.as_deref() == Some(peer_id))
    }

    pub fn touch(&mut self, session_id: u32, peer_id: &str, now_ms: u64) -> bool {
        let Some(s) = self
            .sessions
            .get_mut(&session_id)
            .filter(|s| s.attached_peer.as_deref() == Some(peer_id))
        else {
            return false;
        };
        s.info.last_active_ms = now_ms;
        true
    }

    pub fn detach(&mut self, session_id: u32, peer_id: &str) -> bool {
        self.sessions
            .get_mut(&session_id)
            .is_some_and(|s| s.detach(peer_id))
    }

    pub fn disconnect_peer(&mut self, peer_id: &str) -> Vec<u32> {
        self.sessions
            .iter_mut()
            .filter_map(|(&id, s)| s.detach(peer_id).then_some(id))
            .collect()
    }

    pub fn close_by_peer(&mut self, session_id: u32, peer_id: &str) -> bool {
        if self
            .sessions
            .get(&session_id)
            .and_then(|s| s.attached_peer.as_deref())
            .is_some_and(|owner| owner != peer_id)
        {
            return false;
        }
        self.sessions.remove(&session_id).is_some()
    }

    pub fn remove_if_generation(&mut self, session_id: u32, generation: u64) -> bool {
        if self
            .sessions
            .get(&session_id)
            .is_none_or(|s| s.generation != generation)
        {
            return false;
        }
        self.sessions.remove(&session_id);
        true
    }

    pub fn rename(&mut self, session_id: u32, name: String) -> bool {
        let Some(s) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        s.info.name = name;
        true
    }

    pub fn list(&self) -> Vec<TermSessionInfo> {
        self.sessions.values().map(|s| s.info.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::control::decode_message;

    #[test]
    fn term_header_matches_wire_and_payload_stays_private() {
        let frame = TermFrame {
            version: 1,
            frame_type: 200,
            flags: [2, 128],
            session_id: 0x11223344,
            payload: vec![251, 0, 253],
        };
        let wire = encode_term_frame(&frame);
        assert_eq!(
            wire,
            b"TERM\x01\xc8\x02\x80\x44\x33\x22\x11\x03\0\0\0\xfb\0\xfd"
        );
        assert!(is_term_frame(&wire));
        assert_eq!(decode_term_frame(&wire).unwrap(), frame);
        assert_eq!(
            format!("{frame:?}"),
            "TermFrame { version: 1, frame_type: 200, flags: [2, 128], session_id: 287454020, payload_len: 3 }"
        );
    }

    #[test]
    fn validates_term_header_before_copying_payload() {
        use TermDecodeError::*;
        let valid = *b"TERM\x01\0\0\0\0\0\0\0\0\0\0\0";
        for n in 0..TERM_HEADER_LEN {
            assert!(!is_term_frame(&valid[..n]));
            assert_eq!(decode_term_frame(&valid[..n]), Err(TooShort));
        }
        let mut wire = valid;
        wire[0] = 0;
        assert!(!is_term_frame(&wire));
        assert_eq!(decode_term_frame(&wire), Err(InvalidMagic));
        let mut wire = valid;
        wire[4] = 9;
        assert!(is_term_frame(&wire));
        assert_eq!(decode_term_frame(&wire), Err(UnsupportedVersion(9)));
        let mut wire = valid.to_vec();
        assert!(decode_term_frame(&wire).unwrap().payload.is_empty());
        wire.push(42);
        assert_eq!(
            decode_term_frame(&wire),
            Err(LengthMismatch {
                expected: 0,
                actual: 1
            })
        );
        wire[12..16].copy_from_slice(&(MAX_TERM_PAYLOAD_BYTES as u32).to_le_bytes());
        assert_eq!(
            decode_term_frame(&wire),
            Err(LengthMismatch {
                expected: MAX_TERM_PAYLOAD_BYTES,
                actual: 1
            })
        );
        wire.resize(TERM_HEADER_LEN + MAX_TERM_PAYLOAD_BYTES, 0);
        assert_eq!(
            decode_term_frame(&wire).unwrap().payload.len(),
            MAX_TERM_PAYLOAD_BYTES
        );
        wire[12..16].copy_from_slice(&(MAX_TERM_PAYLOAD_BYTES as u32 + 1).to_le_bytes());
        assert_eq!(
            decode_term_frame(&wire[..16]),
            Err(PayloadTooLarge(MAX_TERM_PAYLOAD_BYTES + 1))
        );
    }

    #[test]
    fn close_payload_selects_target_independently_of_header() {
        let header_id = 777;
        for target in [0, 42, u32::MAX] {
            let json = serde_json::json!({"session_id":target}).to_string();
            assert_eq!(
                resolve_close_target_session(header_id, json.as_bytes()),
                target
            );
        }
        for payload in [
            "",
            "{}",
            "[5]",
            r#"{"session_id":-1}"#,
            r#"{"session_id":1.5}"#,
            r#"{"session_id":"5"}"#,
            r#"{"session_id":4294967296}"#,
        ] {
            assert_eq!(
                resolve_close_target_session(header_id, payload.as_bytes()),
                header_id
            );
        }
    }

    #[test]
    fn terminal_json_defaults_and_debug_redaction() {
        let options: TermOpenOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(options, TermOpenOptions::default());
        assert_eq!(
            serde_json::to_value(&options).unwrap(),
            serde_json::json!({"cols":0,"rows":0})
        );
        let options = TermOpenOptions {
            shell: "secret-shell".into(),
            option: "secret-option".into(),
            ..options
        };
        assert!(!format!("{options:?}").contains("secret"));
        let info: TermSessionInfo =
            serde_json::from_str(r#"{"session_id":9,"shell":"secret-shell","name":"secret-name"}"#)
                .unwrap();
        assert!(!info.attached);
        assert_eq!(info.last_active_ms, 0);
        assert!(!format!("{info:?}").contains("secret"));
    }

    #[test]
    fn terminal_rpcs_and_session_changed_roundtrip() {
        use TerminalControlMessage::*;
        let (seq, timestamp, request_id, session_id, code) = (11, 22, 33, 44, -7);
        for (wire, expected) in [
            (
                encode_check_env_request(seq, timestamp, request_id),
                CheckEnvRequest {
                    seq,
                    timestamp,
                    request_id,
                },
            ),
            (
                encode_check_env_response(seq, timestamp, request_id, code, "secret-error"),
                CheckEnvResponse {
                    seq,
                    timestamp,
                    request_id,
                    code,
                    message: "secret-error".into(),
                },
            ),
            (
                encode_rename_session_request(
                    seq,
                    timestamp,
                    request_id,
                    session_id,
                    "secret-name",
                ),
                RenameSessionRequest {
                    seq,
                    timestamp,
                    request_id,
                    session_id,
                    name: "secret-name".into(),
                },
            ),
            (
                encode_rename_session_response(
                    seq,
                    timestamp,
                    request_id,
                    session_id,
                    code,
                    "secret-detail",
                ),
                RenameSessionResponse {
                    seq,
                    timestamp,
                    request_id,
                    session_id,
                    code,
                    message: "secret-detail".into(),
                },
            ),
            (
                encode_session_changed(seq, timestamp),
                SessionChanged { seq, timestamp },
            ),
        ] {
            let decoded = extract_terminal_control(&decode_message(&wire).unwrap()).unwrap();
            assert_eq!(decoded, expected);
            assert!(!format!("{decoded:?}").contains("secret"));
        }
    }

    #[test]
    fn takeover_blocks_stale_peer_even_during_disconnect() {
        let mut r = TerminalRegistry::new();
        for id in [20, 10] {
            r.create_session(id, "old", "one".into(), "sh".into(), 5);
        }
        assert!(r.attach(20, "new", 10));
        assert!(!r.is_owner(20, "old"));
        assert!(!r.touch(20, "old", 15));
        assert!(!r.detach(20, "old"));
        assert!(!r.close_by_peer(20, "old"));
        assert_eq!(r.disconnect_peer("old"), [10]);
        assert!(r.is_owner(20, "new"));
        assert!(r.touch(20, "new", 18));
        assert!(r.rename(20, "two".into()));
        let list = r.list();
        assert_eq!(list[0].session_id, 10);
        assert!(!list[0].attached);
        assert_eq!(
            list[1],
            TermSessionInfo {
                session_id: 20,
                name: "two".into(),
                shell: "sh".into(),
                attached: true,
                last_active_ms: 18
            }
        );
        assert!(r.close_by_peer(10, "other"));
        assert!(r.detach(20, "new"));
        assert!(!r.is_owner(20, "new"));
        assert!(r.attach(20, "third", 19));
        assert!(r.close_by_peer(20, "third"));
        assert!(r.list().is_empty());
    }

    #[test]
    fn an_exiting_reader_cannot_remove_a_recreated_session() {
        let mut r = TerminalRegistry::default();
        let old = r.create_session(9, "a", String::new(), String::new(), 1);
        assert!(r.close_by_peer(9, "a"));
        let new = r.create_session(9, "b", String::new(), String::new(), 2);
        assert!(new > old);
        assert!(!r.remove_if_generation(9, old));
        assert!(r.is_owner(9, "b"));
        assert!(r.remove_if_generation(9, new));
        assert!(!r.remove_if_generation(9, new));
    }
}
