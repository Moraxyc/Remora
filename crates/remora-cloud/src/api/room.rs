//! Room ticket issuance and HTTP REST endpoints (`/api/v1/room/create`,
//! `/api/v1/room/join/by_device/{device_id}`).

use std::sync::Arc;

use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use crate::PublicUrl;
use crate::api::device::{CallerDevice, CallerVersion, Devices};
use crate::api::user::User;
use crate::api::{self, ApiError, Body};
use crate::contracts::{Room, TokenKey};
use crate::room::RoomClient;

/// How long clients should wait for the signaling connection to come up.
const WS_CONNECT_TIMEOUT_MS: u32 = 16_000;
/// Maximum interval in seconds between signaling `refresh_reconnect_key` calls.
const MAX_RECONNECT_DELTA_SECS: u32 = 300;
/// Retry delay in milliseconds for client streamer reconnection.
const STREAMER_RETRY_DELTA_MS: u32 = 1_000;

/// Issues room tokens and says where to use them.
#[derive(Clone)]
pub struct Tickets {
    pub tokens: Arc<TokenKey>,
    pub public_url: PublicUrl,
}

impl Tickets {
    /// A room token and the signaling URL it is for.
    pub(crate) fn issue(&self, room: Room) -> (String, String) {
        (self.tokens.issue_room(room), self.public_url.signaling())
    }
}

#[derive(Serialize)]
pub struct CreateResponse {
    signaling_server: String,
    signaling_list: [String; 1],
    sni_required_signaling_list: Vec<String>,
    streamer_config: &'static str,
    token: String,
    ws_connect_timeout_ms: u32,
    max_reconnect_delta: u32,
    streamer_retry_delta_ms: u32,
    international_connect: bool,
}

/// `POST /api/v1/room/create`, for a logged-in host on its registered device, which becomes the
/// user's. Official hosts send `{"last_controlled_interval": -1}` or `{}`.
pub async fn create(
    user: User,
    State(devices): State<Devices>,
    State(tickets): State<Tickets>,
    CallerDevice(device_id): CallerDevice,
    CallerVersion(version_name): CallerVersion,
) -> Result<api::Ok<CreateResponse>, ApiError> {
    let device_id = device_id.ok_or(ApiError::INVALID_REQUEST)?;
    if !devices.bind(&device_id, &user)? {
        return Err(ApiError::INVALID_REQUEST);
    }
    if let Some(version_name) = version_name.as_deref() {
        devices.set_version_name(&device_id, version_name)?;
    }

    tracing::info!(user_id = %user.id, %device_id, "room created");
    let (token, signaling_server) = tickets.issue(Room::host(user.id, device_id));
    let sni_required_signaling_list = if tickets.public_url.requires_sni() {
        vec![signaling_server.clone()]
    } else {
        Vec::new()
    };
    Ok(api::Ok(CreateResponse {
        signaling_list: [signaling_server.clone()],
        signaling_server,
        sni_required_signaling_list,
        streamer_config: "{}",
        token,
        ws_connect_timeout_ms: WS_CONNECT_TIMEOUT_MS,
        max_reconnect_delta: MAX_RECONNECT_DELTA_SECS,
        streamer_retry_delta_ms: STREAMER_RETRY_DELTA_MS,
        international_connect: false,
    }))
}

#[derive(Serialize)]
pub struct JoinResponse {
    signaling_server: String,
    signaling_list: [String; 1],
    sni_required_signaling_list: Vec<String>,
    streamer_config: &'static str,
    token: String,
    ws_connect_timeout_ms: u32,
    max_reconnect_delta: u32,
    streamer_retry_delta_ms: u32,
    international_connect: bool,
    /// Where the official clients send connection reports. Remora collects none.
    report_token: &'static str,
    report_url: &'static str,
    report_server_address: &'static str,
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
    let (token, signaling_server) = tickets.issue(Room::viewer(
        user.id,
        device_id,
        from_device,
        request.force_join,
    ));
    let sni_required_signaling_list = if tickets.public_url.requires_sni() {
        vec![signaling_server.clone()]
    } else {
        Vec::new()
    };
    Ok(api::Ok(JoinResponse {
        signaling_list: [signaling_server.clone()],
        signaling_server,
        sni_required_signaling_list,
        streamer_config: "{}",
        token,
        ws_connect_timeout_ms: WS_CONNECT_TIMEOUT_MS,
        max_reconnect_delta: MAX_RECONNECT_DELTA_SECS,
        streamer_retry_delta_ms: STREAMER_RETRY_DELTA_MS,
        international_connect: false,
        report_token: "",
        report_url: "",
        report_server_address: "",
        publisher_platform,
    }))
}
