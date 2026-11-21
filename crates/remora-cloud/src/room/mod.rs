//! Rooms: the tickets to signaling, for a device's host and for viewers joining it, plus
//! the bus-backed client API handlers use to query and control rooms.

pub(crate) mod registry;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::PublicUrl;
use crate::api::auth::token::{Role, Room, TokenKey};
use crate::api::device::{CallerDevice, Devices};
use crate::api::user::User;
use crate::api::{self, ApiError, Body};
use crate::bus::{Bus, BusMessage, Reply, RoomCommand, TOPIC_ROOM_COMMAND};

/// How long clients should wait for the signaling connection to come up.
const WS_CONNECT_TIMEOUT_MS: u32 = 10_000;

/// How long the sessions of a host that left signaling wait for it to reconnect before they
/// end. Hosts wait a few seconds and ask for a new room before reconnecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectGrace(pub Duration);

impl Default for ReconnectGrace {
    fn default() -> Self {
        Self(Duration::from_secs(30))
    }
}

/// Bus-backed handle used by control-plane handlers to query or instruct room actors without
/// holding direct references to room state.
#[derive(Clone)]
pub struct RoomClient {
    bus: Arc<dyn Bus>,
}

impl RoomClient {
    pub fn new(bus: Arc<dyn Bus>) -> Self {
        Self { bus }
    }

    pub async fn is_online(&self, device_id: &str) -> bool {
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::IsOnline {
                    device_id: device_id.to_owned(),
                    reply,
                }),
            )
            .await;
        if delivered == 0 {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub async fn online_devices(&self, device_ids: Vec<String>) -> HashSet<String> {
        if device_ids.is_empty() {
            return HashSet::new();
        }
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::OnlineDevices { device_ids, reply }),
            )
            .await;
        if delivered == 0 {
            return HashSet::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn controlled_elsewhere(&self, device_id: &str, from_device: Option<&str>) -> bool {
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::ControlledElsewhere {
                    device_id: device_id.to_owned(),
                    from_device: from_device.map(str::to_owned),
                    reply,
                }),
            )
            .await;
        if delivered == 0 {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub async fn kick_host(&self, device_id: &str) {
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::KickHost {
                    device_id: device_id.to_owned(),
                    reply,
                }),
            )
            .await;
        if delivered > 0 {
            let _ = rx.await;
        }
    }

    pub async fn push_to_host(&self, device_id: &str, name: &str, body: Value) -> bool {
        let (reply, rx) = Reply::channel();
        let delivered = self
            .bus
            .publish(
                TOPIC_ROOM_COMMAND,
                BusMessage::RoomCommand(RoomCommand::PushToHost {
                    device_id: device_id.to_owned(),
                    name: name.to_owned(),
                    body,
                    reply,
                }),
            )
            .await;
        if delivered == 0 {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

/// Issues room tokens and says where to use them.
#[derive(Clone)]
pub struct Tickets {
    pub tokens: Arc<TokenKey>,
    pub public_url: PublicUrl,
}

impl Tickets {
    /// A room token and the signaling URL it is for.
    fn issue(&self, room: Room) -> (String, String) {
        (self.tokens.issue_room(room), self.public_url.signaling())
    }
}

#[derive(Serialize)]
pub struct CreateResponse {
    signaling_server: String,
    signaling_list: [String; 1],
    token: String,
    ws_connect_timeout_ms: u32,
}

/// `POST /api/v1/room/create`, for a logged-in host on its registered device, which becomes the
/// user's. The body (`{}`) carries nothing.
pub async fn create(
    user: User,
    State(devices): State<Devices>,
    State(tickets): State<Tickets>,
    CallerDevice(device_id): CallerDevice,
) -> Result<api::Ok<CreateResponse>, ApiError> {
    let device_id = device_id.ok_or(ApiError::INVALID_REQUEST)?;
    if !devices.bind(&device_id, &user)? {
        return Err(ApiError::INVALID_REQUEST);
    }

    tracing::info!(user_id = %user.id, %device_id, "room created");
    let (token, signaling_server) = tickets.issue(Room {
        sub: user.id,
        device_id,
        role: Role::Host,
    });
    Ok(api::Ok(CreateResponse {
        signaling_list: [signaling_server.clone()],
        signaling_server,
        token,
        ws_connect_timeout_ms: WS_CONNECT_TIMEOUT_MS,
    }))
}

#[derive(Serialize)]
pub struct JoinResponse {
    signaling_list: [String; 1],
    token: String,
    ws_connect_timeout_ms: u32,
    /// Where the official clients send connection reports. Remora collects none.
    report_token: &'static str,
    report_url: &'static str,
    /// The host's platform, which decides the viewer's keyboard mapping.
    publisher_platform: u8,
}

#[derive(Deserialize)]
pub struct JoinRequest {
    /// Take the desktop over from another device that controls it.
    #[serde(default)]
    force_join: bool,
}

/// `POST /api/v1/room/join/by_device/{device_id}`: a viewer's ticket into the room of one of the
/// user's devices whose host is online, unless another device controls its desktop.
pub async fn join_by_device(
    user: User,
    Path(device_id): Path<String>,
    State(devices): State<Devices>,
    State(rooms): State<RoomClient>,
    State(tickets): State<Tickets>,
    CallerDevice(from_device): CallerDevice,
    Body(request): Body<JoinRequest>,
) -> Result<api::Ok<JoinResponse>, ApiError> {
    let publisher_platform = devices
        .platform_of_owned(&device_id, &user.id)?
        .ok_or(ApiError::UNKNOWN_DEVICE)?;
    if !rooms.is_online(&device_id).await {
        return Err(ApiError::DEVICE_OFFLINE);
    }
    if !request.force_join
        && rooms
            .controlled_elsewhere(&device_id, from_device.as_deref())
            .await
    {
        return Err(ApiError::BEING_CONTROLLED);
    }

    tracing::info!(user_id = %user.id, %device_id, take_over = request.force_join, "room joined");
    let (token, signaling_server) = tickets.issue(Room {
        sub: user.id,
        device_id,
        role: Role::Viewer {
            from_device,
            take_over: request.force_join,
        },
    });
    Ok(api::Ok(JoinResponse {
        signaling_list: [signaling_server],
        token,
        ws_connect_timeout_ms: WS_CONNECT_TIMEOUT_MS,
        report_token: "",
        report_url: "",
        publisher_platform,
    }))
}
