//! Self-hostable stand-in for the UU Remote cloud.

mod api;
mod device;
mod login;
mod signing;
mod token;
mod user;

use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use remora_proto::cloud::SignKey;

/// Everything the endpoints share. All of it lives in memory for now.
#[derive(Clone, FromRef)]
struct Cloud {
    devices: device::Devices,
    logins: login::Logins,
    tokens: Arc<token::TokenKey>,
}

/// All REST endpoints, with fresh state. With a `sign_key`, every request must carry a valid
/// `x-param-sign`; without one, signatures are not checked.
pub fn router(sign_key: Option<SignKey>) -> Router {
    let cloud = Cloud {
        devices: Default::default(),
        logins: Default::default(),
        tokens: Arc::new(token::TokenKey::generate()),
    };
    let app = Router::new()
        .route("/api/v1/device/windows/init", post(device::windows_init))
        .route("/api/v1/qrcode/gen/login", post(login::generate))
        .route("/api/v1/qrcode/login/status", post(login::status))
        .route("/api/v1/login/by_qrcode", post(login::by_qrcode))
        .route("/api/v1/user/info", get(user::info))
        .with_state(cloud);
    match sign_key {
        Some(key) => app.layer(from_fn_with_state(
            Arc::new(key),
            signing::require_signature,
        )),
        None => app,
    }
}
