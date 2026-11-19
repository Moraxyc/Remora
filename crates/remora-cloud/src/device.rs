//! Devices: registration, ownership and the device list.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{self, ApiError, Body};
use crate::signaling::Hosts;
use crate::user::User;

/// Identifies the calling installation once it has registered.
pub const DEVICE_ID_HEADER: &str = "x-param-device-id";

/// What `device/windows/init` registers as, in UU's platform numbering.
const PLATFORM_WINDOWS: u8 = 1;

/// Registered devices, by device id.
#[derive(Debug, Default, Clone)]
pub struct Devices(Arc<Mutex<HashMap<String, Device>>>);

#[derive(Debug)]
struct Device {
    /// The installation that registered it.
    client_id: String,
    name: String,
    platform: u8,
    /// The user whose host runs on it, once it has created a room.
    owner: Option<String>,
}

impl Devices {
    /// Registering the same installation again returns its existing device instead of a new
    /// one, renamed to `name`.
    fn register(&self, client_id: String, name: String, platform: u8) -> String {
        let mut devices = self.0.lock().expect("device lock poisoned");
        if let Some((id, device)) = devices.iter_mut().find(|(_, d)| d.client_id == client_id) {
            device.name = name;
            return id.clone();
        }
        let id = Uuid::new_v4().simple().to_string();
        let device = Device {
            client_id,
            name,
            platform,
            owner: None,
        };
        devices.insert(id.clone(), device);
        id
    }

    /// Makes `user_id` the owner of a registered device; `false` if there is no such device.
    pub fn bind(&self, device_id: &str, user_id: &str) -> bool {
        let mut devices = self.0.lock().expect("device lock poisoned");
        let Some(device) = devices.get_mut(device_id) else {
            return false;
        };
        device.owner = Some(user_id.to_owned());
        true
    }
}

/// Body of `POST /api/v1/device/windows/init`. The hardware summary that clients also send is
/// ignored.
#[derive(Deserialize)]
pub struct InitRequest {
    client_id: String,
    #[serde(default)]
    name: String,
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
    let device_id = devices.register(request.client_id, request.name, PLATFORM_WINDOWS);
    tracing::info!(%device_id, "device registered");
    Ok(api::Ok(InitResponse { device_id }))
}

fn entry(id: &str, device: &Device, hosts: &Hosts) -> Entry {
    Entry {
        device_id: id.to_owned(),
        alias: (!device.name.is_empty()).then(|| device.name.clone()),
        platform: device.platform,
        status: if hosts.is_online(id) {
            Status::Connected
        } else {
            Status::Disconnected
        },
    }
}

#[derive(Serialize)]
pub struct Entry {
    device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    platform: u8,
    status: Status,
}

/// Whether the device's host is on signaling right now.
#[derive(Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Status {
    Connected,
    Disconnected,
}

#[derive(Serialize)]
pub struct ListResponse {
    /// The calling device, when it is registered.
    current_device: Option<Entry>,
    my_binded_devices: Vec<Entry>,
    /// Sharing devices between users is not supported.
    others_shared_devices: [Entry; 0],
}

/// `GET /api/v1/device/list`: the user's devices.
pub async fn list(
    user: User,
    State(devices): State<Devices>,
    State(hosts): State<Hosts>,
    headers: HeaderMap,
) -> api::Ok<ListResponse> {
    let caller = headers
        .get(DEVICE_ID_HEADER)
        .and_then(|value| value.to_str().ok());
    let registry = devices.0.lock().expect("device lock poisoned");
    let current_device = caller
        .and_then(|id| registry.get_key_value(id))
        .map(|(id, device)| entry(id, device, &hosts));
    let mut my_binded_devices: Vec<Entry> = registry
        .iter()
        .filter(|(_, device)| device.owner.as_deref() == Some(user.id.as_str()))
        .map(|(id, device)| entry(id, device, &hosts))
        .collect();
    my_binded_devices.sort_unstable_by(|a, b| a.device_id.cmp(&b.device_id));
    api::Ok(ListResponse {
        current_device,
        my_binded_devices,
        others_shared_devices: [],
    })
}
