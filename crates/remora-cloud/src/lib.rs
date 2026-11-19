//! Self-hostable stand-in for the UU Remote cloud.

mod api;
mod device;
mod signing;

use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::routing::post;
use remora_proto::cloud::SignKey;

pub use device::Devices;

/// All REST endpoints, sharing one device registry. With a `sign_key`, every request must carry
/// a valid `x-param-sign`; without one, signatures are not checked.
pub fn router(devices: Devices, sign_key: Option<SignKey>) -> Router {
    let app = Router::new()
        .route("/api/v1/device/windows/init", post(device::windows_init))
        .with_state(devices);
    match sign_key {
        Some(key) => app.layer(from_fn_with_state(
            Arc::new(key),
            signing::require_signature,
        )),
        None => app,
    }
}
