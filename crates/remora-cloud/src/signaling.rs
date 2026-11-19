//! Socket.IO signaling.
//!
//! Hosts and viewers connect with a room token in `X-NRD-AUTH`. A host asks for `room_info`
//! and from then on counts as online; a viewer sends `control` to open a session with it.
//! Routing between them lives in [`crate::rooms`].

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use remora_proto::socketio::{
    Decoder, Event, Handshake, PING, Packet, encode_ack, encode_connected, encode_event,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};

use crate::rooms::{Frame, Outbound, Peer, Refused, Rooms};
use crate::token::{Role, Room, TokenKey, random};

const AUTH_HEADER: &str = "x-nrd-auth";

/// Room for bursts of relayed candidates before a slow connection starts losing frames.
const OUTBOX_CAPACITY: usize = 256;

/// Events passed between the host and a viewer in session with it.
const RELAYED_EVENTS: [&str; 2] = ["soac", "forward_setting"];

/// Engine.IO heartbeat: the server pings every `interval` and drops a client that has not
/// answered within `timeout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ping {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Default for Ping {
    /// Values UU clients accept.
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(25),
            timeout: Duration::from_secs(20),
        }
    }
}

#[derive(Deserialize)]
pub struct EngineQuery {
    #[serde(rename = "EIO")]
    eio: String,
    transport: String,
}

/// `GET /socket.io/?EIO=4&transport=websocket`.
pub async fn connect(
    upgrade: WebSocketUpgrade,
    Query(query): Query<EngineQuery>,
    State(tokens): State<Arc<TokenKey>>,
    State(rooms): State<Rooms>,
    State(ping): State<Ping>,
    headers: HeaderMap,
) -> Response {
    if query.eio != "4" || query.transport != "websocket" {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let room = headers
        .get(AUTH_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|token| tokens.verify_room(token));
    let Some(room) = room else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    upgrade.on_upgrade(move |socket| serve(socket, room, rooms, ping))
}

async fn serve(socket: WebSocket, room: Room, rooms: Rooms, ping: Ping) {
    let id = hex::encode(random::<16>());
    let (outbox, mut inbox) = mpsc::channel(OUTBOX_CAPACITY);
    rooms.join(
        &room,
        Peer {
            id: id.clone(),
            outbox,
        },
    );
    let device_id = room.device_id.clone();
    let role = room.role;
    tracing::info!(%device_id, ?role, "signaling connected");

    let open = Handshake {
        sid: id.clone(),
        ping_interval: millis(ping.interval),
        ping_timeout: millis(ping.timeout),
    };
    let mut session = Session {
        socket,
        decoder: Decoder::new(),
        id: id.clone(),
        room,
        rooms: rooms.clone(),
    };
    let reason = session.run(open, &mut inbox, ping).await;

    rooms.leave(&session.room, &id);
    tracing::info!(%device_id, ?role, reason, "signaling closed");
}

struct Session {
    socket: WebSocket,
    decoder: Decoder,
    /// This connection's `client_id`, also its Engine.IO session id.
    id: String,
    room: Room,
    rooms: Rooms,
}

impl Session {
    /// Serves the connection until it ends, and says why.
    async fn run(
        &mut self,
        open: Handshake,
        inbox: &mut mpsc::Receiver<Outbound>,
        ping: Ping,
    ) -> &'static str {
        if self.send(Frame::Text(open.encode())).await.is_err() {
            return "send failed";
        }
        let mut next_ping = Instant::now() + ping.interval;
        let mut pong_deadline = None;
        loop {
            tokio::select! {
                frame = self.socket.recv() => {
                    let packet = match frame {
                        Some(Ok(Message::Text(text))) => self.decoder.text(&text),
                        Some(Ok(Message::Binary(bytes))) => self.decoder.binary(&bytes),
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                        Some(Ok(Message::Close(_))) | None => return "closed by peer",
                        Some(Err(_)) => return "receive failed",
                    };
                    let reply = match packet {
                        Ok(Some(Packet::Pong)) => {
                            pong_deadline = None;
                            continue;
                        }
                        Ok(Some(Packet::Close)) => return "closed by peer",
                        Ok(Some(packet)) => self.answer(packet),
                        Ok(None) => continue,
                        Err(_) => return "protocol error",
                    };
                    for frame in reply {
                        if self.send(Frame::Text(frame)).await.is_err() {
                            return "send failed";
                        }
                    }
                }
                outbound = inbox.recv() => match outbound {
                    Some(Outbound::Frames(frames)) => {
                        for frame in frames {
                            if self.send(frame).await.is_err() {
                                return "send failed";
                            }
                        }
                    }
                    Some(Outbound::Kicked) | None => {
                        // `left` without an `ice_id` tells the host it lost the room.
                        let left = encode_event("left", &[json!({})], None);
                        let _ = self.send(Frame::Text(left)).await;
                        return "replaced by a newer connection";
                    }
                },
                () = sleep_until(pong_deadline.unwrap_or(next_ping)) => {
                    if pong_deadline.is_some() {
                        return "ping timeout";
                    }
                    if self.send(Frame::Text(PING.to_owned())).await.is_err() {
                        return "send failed";
                    }
                    let now = Instant::now();
                    pong_deadline = Some(now + ping.timeout);
                    next_ping = now + ping.interval;
                }
            }
        }
    }

    /// Frames to send back in answer to one packet.
    fn answer(&self, packet: Packet) -> Vec<String> {
        match packet {
            Packet::Connected => vec![encode_connected(&self.id)],
            Packet::Event(event) => self.answer_event(event),
            _ => Vec::new(),
        }
    }

    fn answer_event(&self, mut event: Event) -> Vec<String> {
        let ack = |args: &[Value]| event.ack_id.map(|id| encode_ack(id, args));
        match (self.room.role, event.name.as_str()) {
            (Role::Host, "room_info") => {
                let ready = json!({ "data": { "status": "online", "publisher_availability_status": "ready" } });
                ack(&[json!({})])
                    .into_iter()
                    .chain([encode_event("bmsg_push", &[ready], None)])
                    .collect()
            }
            (_, "refresh_reconnect_key") => {
                let key = hex::encode(random::<16>());
                ack(&[json!("success"), json!({ "reconnect_key": key })])
                    .into_iter()
                    .collect()
            }
            (Role::Viewer, "control") => {
                let header = event.args.first().cloned().unwrap_or_default();
                let options = header
                    .get("app_data")
                    .and_then(|value| event.attachment(value))
                    .unwrap_or_default();
                let result = self
                    .rooms
                    .control(&self.room.device_id, &self.id, &header, options);
                let reply = match result {
                    Ok(granted) => {
                        tracing::info!(device_id = %self.room.device_id, "control granted");
                        [json!("success"), granted]
                    }
                    Err(Refused::HostOffline) => {
                        tracing::info!(device_id = %self.room.device_id, "control refused: host offline");
                        [json!("failure"), json!({ "reason": "host offline" })]
                    }
                };
                ack(&reply).into_iter().collect()
            }
            (_, name) if RELAYED_EVENTS.contains(&name) => {
                let body = event.args.drain(..).next().unwrap_or_default();
                if !self.rooms.relay(&self.room, &self.id, name, body) {
                    tracing::debug!(event = name, "dropped an event outside any session");
                }
                Vec::new()
            }
            (_, name) => {
                tracing::debug!(event = name, "ignored event");
                Vec::new()
            }
        }
    }

    async fn send(&mut self, frame: Frame) -> Result<(), axum::Error> {
        let message = match frame {
            Frame::Text(text) => Message::Text(text.into()),
            Frame::Binary(bytes) => Message::Binary(bytes.into()),
        };
        self.socket.send(message).await
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
