//! Device registration.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{self, ApiError, Body};

/// Device ids of registered installations, keyed by `client_id`.
#[derive(Debug, Default, Clone)]
pub struct Devices(Arc<Mutex<HashMap<String, String>>>);

impl Devices {
    /// Registering the same installation again returns its existing device instead of a new one.
    fn register(&self, client_id: String) -> String {
        let mut devices = self.0.lock().expect("device lock poisoned");
        devices
            .entry(client_id)
            .or_insert_with(|| Uuid::new_v4().simple().to_string())
            .clone()
    }
}

/// Body of `POST /api/v1/device/windows/init`. The name and hardware summary that clients also
/// send are ignored for now.
#[derive(Deserialize)]
pub struct InitRequest {
    client_id: String,
}

#[derive(Serialize)]
pub struct InitResponse {
    device_id: String,
}

pub async fn windows_init(
    State(devices): State<Devices>,
    Body(request): Body<InitRequest>,
) -> Result<api::Ok<InitResponse>, ApiError> {
    if request.client_id.is_empty() {
        return Err(ApiError::INVALID_REQUEST);
    }
    let device_id = devices.register(request.client_id);
    tracing::info!(%device_id, "device registered");
    Ok(api::Ok(InitResponse { device_id }))
}
