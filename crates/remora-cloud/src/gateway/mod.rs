//! Socket.IO signaling gateway.
//!
//! Manages WebSocket connections, Engine.IO heartbeats, client profile detection, and online
//! presence. All room events are forwarded to the room role over the internal bus, and outbound
//! frames arrive back from the bus as `ToEndpoint`.

pub(crate) mod presence;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use remora_proto::profile::{Profile, detect_from_headers};
use remora_proto::signal::{
    Decoder, Handshake, PING, Packet, SignalEvent, encode_connected, encode_event,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};

use crate::api::ApiError;
use crate::api::auth::token::{Room, TokenKey, random};
use crate::api::device::Devices;
use crate::api::share::Shares;
use crate::bus::{
    Bus, BusMessage, EndpointFrame, PresenceChanged, Reply, RoomCommand, TOPIC_PRESENCE,
    TOPIC_ROOM_COMMAND,
};
use crate::gateway::presence::{Connections, Outbound};

const AUTH_HEADER: &str = "x-nrd-auth";

/// Room for bursts of relayed candidates before a slow connection starts losing frames.
const OUTBOX_CAPACITY: usize = 256;

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

/// What admitted connections are served with on this gateway instance.
#[derive(Clone)]
pub struct Switchboard {
    pub bus: Arc<dyn Bus>,
    pub connections: Connections,
    pub ping: Ping,
}

/// Checks the room token a connection brings.
#[derive(Clone)]
pub struct Admission {
    pub tokens: Arc<TokenKey>,
    pub devices: Devices,
    pub shares: Shares,
}

impl Admission {
    /// The room an unexpired token grants, unless its device has been unbound from the token's
    /// user (or its guest/share permission was revoked) since.
    fn admit(&self, token: &str) -> rusqlite::Result<Option<Room>> {
        let Some(room) = self.tokens.verify_room(token) else {
            return Ok(None);
        };
        let allowed = if room.guest {
            self.shares.guest_exists(&room.sub, &room.device_id)?
        } else if room.shared {
            self.shares.is_share_active(&room.device_id)?
        } else {
            self.devices
                .platform_of_owned(&room.device_id, &room.sub)?
                .is_some()
        };
        Ok(allowed.then_some(room))
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
    State(admission): State<Admission>,
    State(switchboard): State<Switchboard>,
    headers: HeaderMap,
) -> Response {
    if query.eio != "4" || query.transport != "websocket" {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let token = headers
        .get(AUTH_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let room = match admission.admit(token) {
        Ok(Some(room)) => room,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(error) => return ApiError::from(error).into_response(),
    };
    let profile = detect_from_headers(
        headers
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
    );
    upgrade.on_upgrade(move |socket| serve(socket, room, profile, switchboard))
}

async fn serve(
    socket: WebSocket,
    room: Room,
    profile: Option<&'static Profile>,
    switchboard: Switchboard,
) {
    let Switchboard {
        bus,
        connections,
        ping,
    } = switchboard;
    let id = hex::encode(random::<16>());
    let (outbox, mut inbox) = mpsc::channel(OUTBOX_CAPACITY);
    connections.insert(id.clone(), outbox);

    let device_id = room.device_id.clone();
    let role = room.role.clone();

    bus.publish(
        TOPIC_PRESENCE,
        BusMessage::PresenceChanged(PresenceChanged {
            device_id: device_id.clone(),
            client_id: id.clone(),
            role: role.clone(),
            online: true,
        }),
    )
    .await;

    let (reply, rx) = Reply::channel();
    let delivered = bus
        .publish(
            TOPIC_ROOM_COMMAND,
            BusMessage::RoomCommand(RoomCommand::Join {
                room: room.clone(),
                client_id: id.clone(),
                profile,
                reply,
            }),
        )
        .await;
    if delivered > 0 {
        let _ = rx.await;
    }

    tracing::info!(
        %device_id,
        ?role,
        profile = profile.map(|p| p.id.as_str()),
        "signaling connected"
    );

    let open = Handshake {
        sid: id.clone(),
        ping_interval: millis(ping.interval),
        ping_timeout: millis(ping.timeout),
    };
    let mut session = Session {
        socket,
        decoder: Decoder::new(),
        id: id.clone(),
        room: room.clone(),
        bus: bus.clone(),
    };
    let reason = session.run(open, &mut inbox, ping).await;

    connections.remove(&id);
    bus.publish(
        TOPIC_PRESENCE,
        BusMessage::PresenceChanged(PresenceChanged {
            device_id: device_id.clone(),
            client_id: id.clone(),
            role: role.clone(),
            online: false,
        }),
    )
    .await;
    bus.publish(
        TOPIC_ROOM_COMMAND,
        BusMessage::RoomCommand(RoomCommand::Leave {
            room,
            client_id: id,
        }),
    )
    .await;
    tracing::info!(%device_id, ?role, reason, "signaling closed");
}

struct Session {
    socket: WebSocket,
    decoder: Decoder,
    /// This connection's `client_id`, also its Engine.IO session id.
    id: String,
    room: Room,
    bus: Arc<dyn Bus>,
}

impl Session {
    /// Serves the connection until it ends, and says why.
    async fn run(
        &mut self,
        open: Handshake,
        inbox: &mut mpsc::Receiver<Outbound>,
        ping: Ping,
    ) -> &'static str {
        if self.send(EndpointFrame::Text(open.encode())).await.is_err() {
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
                    match packet {
                        Ok(Some(Packet::Pong)) => {
                            pong_deadline = None;
                        }
                        Ok(Some(Packet::Close)) => return "closed by peer",
                        Ok(Some(Packet::Connected)) => {
                            if self
                                .send(EndpointFrame::Text(encode_connected(&self.id)))
                                .await
                                .is_err()
                            {
                                return "send failed";
                            }
                        }
                        Ok(Some(Packet::Event(event))) => {
                            let ack_id = event.ack_id;
                            let signal = SignalEvent::from_event(&event);
                            self.bus
                                .publish(
                                    TOPIC_ROOM_COMMAND,
                                    BusMessage::RoomCommand(RoomCommand::EndpointEvent {
                                        room: self.room.clone(),
                                        client_id: self.id.clone(),
                                        ack_id,
                                        event: Box::new(signal),
                                    }),
                                )
                                .await;
                        }
                        Ok(Some(_)) | Ok(None) => {}
                        Err(_) => return "protocol error",
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
                        let _ = self.send(EndpointFrame::Text(left)).await;
                        return "kicked";
                    }
                },
                () = sleep_until(pong_deadline.unwrap_or(next_ping)) => {
                    if pong_deadline.is_some() {
                        return "ping timeout";
                    }
                    if self.send(EndpointFrame::Text(PING.to_owned())).await.is_err() {
                        return "send failed";
                    }
                    let now = Instant::now();
                    pong_deadline = Some(now + ping.timeout);
                    next_ping = now + ping.interval;
                }
            }
        }
    }

    async fn send(&mut self, frame: EndpointFrame) -> Result<(), axum::Error> {
        let message = match frame {
            EndpointFrame::Text(text) => Message::Text(text.into()),
            EndpointFrame::Binary(bytes) => Message::Binary(bytes.into()),
        };
        self.socket.send(message).await
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
