//! Rooms: a host's ticket to its signaling connection.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use serde::Serialize;

use crate::PublicUrl;
use crate::api::{self, ApiError};
use crate::device::{DEVICE_ID_HEADER, Devices};
use crate::token::{Room, TokenKey};
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
    });
    let signaling_server = public_url.signaling();
    Ok(api::Ok(CreateResponse {
        signaling_list: [signaling_server.clone()],
        signaling_server,
        token,
        ws_connect_timeout_ms: WS_CONNECT_TIMEOUT_MS,
    }))
}
