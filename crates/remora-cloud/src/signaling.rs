//! Socket.IO signaling for hosts.
//!
//! A host connects with its room token in `X-NRD-AUTH`, asks for `room_info`, and from then on
//! counts as online. One connection per device: a newer one kicks the older one out.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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

use crate::token::{Room, TokenKey, random};

const AUTH_HEADER: &str = "x-nrd-auth";

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

/// Hosts with a live signaling connection, by device id.
#[derive(Debug, Default, Clone)]
pub struct Hosts(Arc<Mutex<HashMap<String, Host>>>);

#[derive(Debug)]
struct Host {
    connection: u64,
    outbox: mpsc::Sender<Outbound>,
}

#[derive(Debug)]
enum Outbound {
    /// Another connection took over this device.
    Kicked,
}

impl Hosts {
    fn join(&self, device_id: &str, connection: u64, outbox: mpsc::Sender<Outbound>) {
        let previous = self
            .0
            .lock()
            .expect("hosts lock poisoned")
            .insert(device_id.to_owned(), Host { connection, outbox });
        if let Some(previous) = previous {
            // A full outbox means that connection is stuck; it will time out by itself.
            let _ = previous.outbox.try_send(Outbound::Kicked);
        }
    }

    fn leave(&self, device_id: &str, connection: u64) {
        let mut hosts = self.0.lock().expect("hosts lock poisoned");
        if hosts
            .get(device_id)
            .is_some_and(|host| host.connection == connection)
        {
            hosts.remove(device_id);
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
    State(hosts): State<Hosts>,
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
    upgrade.on_upgrade(move |socket| serve(socket, room, hosts, ping))
}

async fn serve(socket: WebSocket, room: Room, hosts: Hosts, ping: Ping) {
    let device_id = room.device_id;
    let connection = u64::from_le_bytes(random());
    let sid = hex::encode(random::<16>());
    let (outbox, mut inbox) = mpsc::channel(4);
    hosts.join(&device_id, connection, outbox);
    tracing::info!(%device_id, "host signaling connected");

    let open = Handshake {
        sid: sid.clone(),
        ping_interval: millis(ping.interval),
        ping_timeout: millis(ping.timeout),
    };
    let mut session = Session {
        socket,
        decoder: Decoder::new(),
        sid,
    };
    let reason = session.run(open, &mut inbox, ping).await;

    hosts.leave(&device_id, connection);
    tracing::info!(%device_id, reason, "host signaling closed");
}

struct Session {
    socket: WebSocket,
    decoder: Decoder,
    sid: String,
}

impl Session {
    /// Serves the connection until it ends, and says why.
    async fn run(
        &mut self,
        open: Handshake,
        inbox: &mut mpsc::Receiver<Outbound>,
        ping: Ping,
    ) -> &'static str {
        if self.send(open.encode()).await.is_err() {
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
                        Some(Ok(Message::Close(_))) | None => return "closed by host",
                        Some(Err(_)) => return "receive failed",
                    };
                    let reply = match packet {
                        Ok(Some(Packet::Pong)) => {
                            pong_deadline = None;
                            continue;
                        }
                        Ok(Some(Packet::Close)) => return "closed by host",
                        Ok(Some(packet)) => self.answer(packet),
                        Ok(None) => continue,
                        Err(_) => return "protocol error",
                    };
                    for frame in reply {
                        if self.send(frame).await.is_err() {
                            return "send failed";
                        }
                    }
                }
                outbound = inbox.recv() => match outbound {
                    Some(Outbound::Kicked) | None => {
                        // `left` without an `ice_id` tells the host it lost the room.
                        let _ = self.send(encode_event("left", &[json!({})], None)).await;
                        return "replaced by a newer connection";
                    }
                },
                () = sleep_until(pong_deadline.unwrap_or(next_ping)) => {
                    if pong_deadline.is_some() {
                        return "ping timeout";
                    }
                    if self.send(PING.to_owned()).await.is_err() {
                        return "send failed";
                    }
                    let now = Instant::now();
                    pong_deadline = Some(now + ping.timeout);
                    next_ping = now + ping.interval;
                }
            }
        }
    }

    /// Frames to send in answer to one packet from the host.
    fn answer(&self, packet: Packet) -> Vec<String> {
        match packet {
            Packet::Connected => vec![encode_connected(&self.sid)],
            Packet::Event(event) => answer_event(event),
            _ => Vec::new(),
        }
    }

    async fn send(&mut self, frame: String) -> Result<(), axum::Error> {
        self.socket.send(Message::Text(frame.into())).await
    }
}

fn answer_event(event: Event) -> Vec<String> {
    let ack = |args: &[Value]| event.ack_id.map(|id| encode_ack(id, args));
    match event.name.as_str() {
        "room_info" => {
            let ready =
                json!({ "data": { "status": "online", "publisher_availability_status": "ready" } });
            ack(&[json!({})])
                .into_iter()
                .chain([encode_event("bmsg_push", &[ready], None)])
                .collect()
        }
        "refresh_reconnect_key" => {
            let key = hex::encode(random::<16>());
            ack(&[json!("success"), json!({ "reconnect_key": key })])
                .into_iter()
                .collect()
        }
        name => {
            tracing::debug!(event = name, "ignored host event");
            Vec::new()
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
