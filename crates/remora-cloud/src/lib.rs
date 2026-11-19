//! Self-hostable stand-in for the UU Remote cloud.

mod api;
mod device;

use axum::Router;
use axum::routing::post;

pub use device::Devices;

/// All REST endpoints, sharing one device registry.
pub fn router(devices: Devices) -> Router {
    Router::new()
        .route("/api/v1/device/windows/init", post(device::windows_init))
        .with_state(devices)
}
