//! Engine.IO v4 / Socket.IO v5 framing over WebSocket, default namespace only.
//!
//! Text frames carry `<engine.io type><socket.io type>[<attachments>-][<ack id>][<json>]`.
//! A binary event announces N attachments, which follow as N binary frames.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

pub const CONNECT: &str = "40";
pub const PONG: &str = "3";

/// Server handshake, the `0{…}` open packet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handshake {
    pub sid: String,
    /// Milliseconds between server pings.
    pub ping_interval: u64,
    /// Milliseconds the server waits for a pong.
    pub ping_timeout: u64,
}

impl Handshake {
    /// How long without a server ping before the connection counts as dead.
    pub fn liveness_timeout(&self) -> Duration {
        let ms = self.ping_interval.saturating_add(self.ping_timeout);
        Duration::from_millis(ms).clamp(Duration::from_secs(5), Duration::from_secs(300))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    Open(Handshake),
    Close,
    Ping,
    Pong,
    Noop,
    /// `40{…}`: the namespace accepted our connect.
    Connected,
    /// `41`: the server dropped the namespace.
    Disconnected,
    /// `44{…}`: the namespace refused our connect.
    ConnectError(Value),
    Event(Event),
    Ack(Ack),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: String,
    pub args: Vec<Value>,
    /// Set when the sender expects an acknowledgement.
    pub ack_id: Option<u64>,
    pub attachments: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ack {
    pub id: u64,
    pub args: Vec<Value>,
    pub attachments: Vec<Vec<u8>>,
}

impl Event {
    /// Resolves a `{"_placeholder":true,"num":n}` value to its binary attachment.
    pub fn attachment(&self, placeholder: &Value) -> Option<&[u8]> {
        if placeholder.get("_placeholder")? != &Value::Bool(true) {
            return None;
        }
        let num = usize::try_from(placeholder.get("num")?.as_u64()?).ok()?;
        self.attachments.get(num).map(Vec::as_slice)
    }
}

/// `42[<ack id>]["name",args…]`.
pub fn encode_event(name: &str, args: &[Value], ack_id: Option<u64>) -> String {
    let mut array = Vec::with_capacity(args.len() + 1);
    array.push(Value::from(name));
    array.extend_from_slice(args);
    let id = ack_id.map(|id| id.to_string()).unwrap_or_default();
    format!("42{id}{}", Value::Array(array))
}

/// Reassembles packets from WebSocket frames.
#[derive(Debug, Default)]
pub struct Decoder {
    awaiting: Option<(Packet, usize)>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `None` while a binary packet still waits for attachments.
    pub fn text(&mut self, frame: &str) -> Result<Option<Packet>, DecodeError> {
        if self.awaiting.take().is_some() {
            return Err(DecodeError("text frame while awaiting attachments"));
        }
        let mut chars = frame.chars();
        let packet = match chars.next() {
            Some('0') => serde_json::from_str(chars.as_str())
                .map(Packet::Open)
                .map_err(|_| DecodeError("invalid handshake"))?,
            Some('1') => Packet::Close,
            Some('2') => Packet::Ping,
            Some('3') => Packet::Pong,
            Some('6') => Packet::Noop,
            Some('4') => return self.message(chars.as_str()),
            Some(_) => return Err(DecodeError("unknown engine.io packet type")),
            None => return Err(DecodeError("empty frame")),
        };
        Ok(Some(packet))
    }

    pub fn binary(&mut self, frame: &[u8]) -> Result<Option<Packet>, DecodeError> {
        let Some((mut packet, remaining)) = self.awaiting.take() else {
            return Err(DecodeError("unexpected binary frame"));
        };
        match &mut packet {
            Packet::Event(Event { attachments, .. }) | Packet::Ack(Ack { attachments, .. }) => {
                attachments.push(frame.to_vec());
            }
            _ => unreachable!("only events and acks carry attachments"),
        }
        if remaining > 1 {
            self.awaiting = Some((packet, remaining - 1));
            return Ok(None);
        }
        Ok(Some(packet))
    }

    fn message(&mut self, body: &str) -> Result<Option<Packet>, DecodeError> {
        let (kind, rest) = body
            .split_at_checked(1)
            .ok_or(DecodeError("empty message"))?;
        let (attachments, rest) = match kind {
            "5" | "6" => {
                let (count, rest) = rest
                    .split_once('-')
                    .ok_or(DecodeError("missing attachment count"))?;
                let count = count
                    .parse()
                    .map_err(|_| DecodeError("invalid attachment count"))?;
                (count, rest)
            }
            _ => (0, rest),
        };
        if rest.starts_with('/') {
            return Err(DecodeError("non-default namespace"));
        }
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let (id, json) = rest.split_at(digits);
        let id = (!id.is_empty())
            .then(|| id.parse::<u64>())
            .transpose()
            .map_err(|_| DecodeError("invalid ack id"))?;

        let packet = match kind {
            "0" => Packet::Connected,
            "1" => Packet::Disconnected,
            "4" => Packet::ConnectError(parse_json(json)?),
            "2" | "5" => {
                let mut args = parse_array(json)?;
                let Some(Value::String(name)) = (!args.is_empty()).then(|| args.remove(0)) else {
                    return Err(DecodeError("event without a name"));
                };
                Packet::Event(Event {
                    name,
                    args,
                    ack_id: id,
                    attachments: Vec::new(),
                })
            }
            "3" | "6" => Packet::Ack(Ack {
                id: id.ok_or(DecodeError("ack without an id"))?,
                args: parse_array(json)?,
                attachments: Vec::new(),
            }),
            _ => return Err(DecodeError("unknown socket.io packet type")),
        };
        if attachments > 0 {
            self.awaiting = Some((packet, attachments));
            return Ok(None);
        }
        Ok(Some(packet))
    }
}

fn parse_json(json: &str) -> Result<Value, DecodeError> {
    serde_json::from_str(json).map_err(|_| DecodeError("invalid json"))
}

fn parse_array(json: &str) -> Result<Vec<Value>, DecodeError> {
    match parse_json(json)? {
        Value::Array(values) => Ok(values),
        _ => Err(DecodeError("expected a json array")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(&'static str);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "socket.io: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(frame: &str) -> Packet {
        Decoder::new().text(frame).unwrap().unwrap()
    }

    #[test]
    fn handshake() {
        let open = r#"0{"sid":"abc","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}"#;
        let Packet::Open(handshake) = decode(open) else {
            panic!("expected open");
        };
        assert_eq!(handshake.sid, "abc");
        assert_eq!(handshake.liveness_timeout(), Duration::from_secs(45));

        let short = Handshake {
            sid: String::new(),
            ping_interval: 100,
            ping_timeout: 100,
        };
        assert_eq!(short.liveness_timeout(), Duration::from_secs(5));
    }

    #[test]
    fn control_packets() {
        assert_eq!(decode("1"), Packet::Close);
        assert_eq!(decode("2"), Packet::Ping);
        assert_eq!(decode("3"), Packet::Pong);
        assert_eq!(decode("6"), Packet::Noop);
        assert_eq!(decode(r#"40{"sid":"x"}"#), Packet::Connected);
        assert_eq!(decode("41"), Packet::Disconnected);
        assert_eq!(
            decode(r#"44{"message":"denied"}"#),
            Packet::ConnectError(json!({"message": "denied"}))
        );
    }

    #[test]
    fn events_and_acks() {
        assert_eq!(
            decode(r#"42["released",{"ice_id":"i"}]"#),
            Packet::Event(Event {
                name: "released".into(),
                args: vec![json!({"ice_id": "i"})],
                ack_id: None,
                attachments: vec![],
            })
        );
        assert_eq!(
            decode(r#"4317["success",{"reconnect_key":"k"}]"#),
            Packet::Ack(Ack {
                id: 17,
                args: vec![json!("success"), json!({"reconnect_key": "k"})],
                attachments: vec![],
            })
        );
    }

    #[test]
    fn binary_event_waits_for_attachments() {
        let mut decoder = Decoder::new();
        let header =
            r#"451-["be-controlled",{"ice_id":"i","app_data":{"_placeholder":true,"num":0}}]"#;
        assert_eq!(decoder.text(header), Ok(None));
        let Some(Packet::Event(event)) = decoder.binary(&[0x08, 0x01]).unwrap() else {
            panic!("expected event");
        };
        assert_eq!(event.name, "be-controlled");
        assert_eq!(
            event.attachment(&event.args[0]["app_data"]),
            Some(&[0x08, 0x01][..])
        );
        assert_eq!(event.attachment(&json!({"num": 0})), None);
    }

    #[test]
    fn binary_ack_with_two_attachments() {
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.text(r#"462-9[{"_placeholder":true,"num":1}]"#),
            Ok(None)
        );
        assert_eq!(decoder.binary(b"a"), Ok(None));
        let Some(Packet::Ack(ack)) = decoder.binary(b"b").unwrap() else {
            panic!("expected ack");
        };
        assert_eq!(
            (ack.id, ack.attachments),
            (9, vec![b"a".to_vec(), b"b".to_vec()])
        );
    }

    #[test]
    fn protocol_violations() {
        let mut decoder = Decoder::new();
        assert!(decoder.binary(b"x").is_err());
        assert!(decoder.text("").is_err());
        assert!(decoder.text("9").is_err());
        assert!(decoder.text(r#"42/admin,["x"]"#).is_err());
        assert!(decoder.text("42[1]").is_err());
        assert!(decoder.text("43[]").is_err());

        decoder.text(r#"451-["e",{}]"#).unwrap();
        assert!(decoder.text("2").is_err());
        assert!(
            decoder.binary(b"late").is_err(),
            "the broken packet was dropped"
        );
    }

    #[test]
    fn encodes_events() {
        assert_eq!(
            encode_event("room_info", &[], Some(3)),
            r#"423["room_info"]"#
        );
        assert_eq!(
            encode_event("ice_finished_log", &[json!({"ice_id": "i"})], None),
            r#"42["ice_finished_log",{"ice_id":"i"}]"#
        );
    }
}
