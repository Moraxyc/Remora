//! Rooms: the tickets to signaling, for a device's host and for viewers joining it.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::Serialize;

use crate::PublicUrl;
use crate::api::{self, ApiError};
use crate::device::{DEVICE_ID_HEADER, Devices};
use crate::rooms::Rooms;
use crate::token::{Role, Room, TokenKey};
use crate::user::User;

/// How long clients should wait for the signaling connection to come up.
const WS_CONNECT_TIMEOUT_MS: u32 = 10_000;

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
    State(tokens): State<Arc<TokenKey>>,
    State(public_url): State<PublicUrl>,
    headers: HeaderMap,
) -> Result<api::Ok<CreateResponse>, ApiError> {
    let device_id = headers
        .get(DEVICE_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|id| devices.bind(id, &user.id))
        .ok_or(ApiError::INVALID_REQUEST)?;

    tracing::info!(user_id = %user.id, %device_id, "room created");
    let token = tokens.issue_room(Room {
        sub: user.id,
        device_id: device_id.to_owned(),
        role: Role::Host,
    });
    let signaling_server = public_url.signaling();
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

/// `POST /api/v1/room/join/by_device/{device_id}`: a viewer's ticket into the room of one of the
/// user's devices whose host is online. The body (`{"force_join": …}`) is not needed: several
/// viewers may control a host at once.
pub async fn join_by_device(
    user: User,
    Path(device_id): Path<String>,
    State(devices): State<Devices>,
    State(rooms): State<Rooms>,
    State(tokens): State<Arc<TokenKey>>,
    State(public_url): State<PublicUrl>,
) -> Result<api::Ok<JoinResponse>, ApiError> {
    let publisher_platform = devices
        .platform_of_owned(&device_id, &user.id)
        .ok_or(ApiError::UNKNOWN_DEVICE)?;
    if !rooms.is_online(&device_id) {
        return Err(ApiError::DEVICE_OFFLINE);
    }

    tracing::info!(user_id = %user.id, %device_id, "room joined");
    let token = tokens.issue_room(Room {
        sub: user.id,
        device_id,
        role: Role::Viewer,
    });
    Ok(api::Ok(JoinResponse {
        signaling_list: [public_url.signaling()],
        token,
        ws_connect_timeout_ms: WS_CONNECT_TIMEOUT_MS,
        report_token: "",
        report_url: "",
        publisher_platform,
    }))
}
