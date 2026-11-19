//! Devices: registration, ownership and the device list.

use std::convert::Infallible;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Database;
use crate::api::{self, ApiError, Body, Empty};
use crate::rooms::Rooms;
use crate::user::User;

/// Identifies the calling installation once it has registered.
const DEVICE_ID_HEADER: &str = "x-param-device-id";

/// The calling installation's device id, if it sent one. Nothing vouches for it: it is the
/// device the caller says it is.
pub struct CallerDevice(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for CallerDevice {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        let id = parts
            .headers
            .get(DEVICE_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        Ok(Self(id))
    }
}

/// What `device/windows/init` registers as, in UU's platform numbering.
const PLATFORM_WINDOWS: u8 = 1;

/// Registered devices. Each is registered by one installation (its `client_id`) and, once its
/// host has created a room, owned by that host's user.
#[derive(Debug, Clone)]
pub struct Devices(Database);

impl From<Database> for Devices {
    fn from(database: Database) -> Self {
        Self(database)
    }
}

/// A registered device.
pub struct Device {
    pub id: String,
    pub name: String,
    /// In UU's platform numbering.
    pub platform: u8,
}

impl Device {
    const COLUMNS: &str = "id, name, platform";

    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            platform: row.get(2)?,
        })
    }
}

impl Devices {
    /// Registering the same installation again returns its existing device instead of a new
    /// one, renamed to `name`.
    fn register(&self, client_id: &str, name: &str, platform: u8) -> rusqlite::Result<String> {
        self.0.connection().query_row(
            "INSERT INTO devices (id, client_id, name, platform) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (client_id) DO UPDATE SET name = excluded.name
             RETURNING id",
            params![
                Uuid::new_v4().simple().to_string(),
                client_id,
                name,
                platform
            ],
            |row| row.get(0),
        )
    }

    /// Makes `user` the owner of a registered device whose host logged in as them, and ties
    /// the host's session to the device; `false` if there is no such device.
    pub fn bind(&self, device_id: &str, user: &User) -> rusqlite::Result<bool> {
        let mut connection = self.0.connection();
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE devices SET owner = ?2 WHERE id = ?1",
            [device_id, &user.id],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        transaction.execute(
            "UPDATE sessions SET device_id = ?1 WHERE id = ?2",
            [device_id, &user.session_id],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    /// Takes one of `user_id`'s devices off them and closes its host's sessions; `false` if it
    /// is not theirs.
    pub fn unbind(&self, device_id: &str, user_id: &str) -> rusqlite::Result<bool> {
        let mut connection = self.0.connection();
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE devices SET owner = NULL WHERE id = ?1 AND owner = ?2",
            [device_id, user_id],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        transaction.execute("DELETE FROM sessions WHERE device_id = ?1", [device_id])?;
        transaction.commit()?;
        Ok(true)
    }

    /// The platform of `device_id`, if it belongs to `user_id`.
    pub fn platform_of_owned(
        &self,
        device_id: &str,
        user_id: &str,
    ) -> rusqlite::Result<Option<u8>> {
        self.0
            .connection()
            .query_row(
                "SELECT platform FROM devices WHERE id = ?1 AND owner = ?2",
                [device_id, user_id],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn get(&self, device_id: &str) -> rusqlite::Result<Option<Device>> {
        let query = format!("SELECT {} FROM devices WHERE id = ?1", Device::COLUMNS);
        self.0
            .connection()
            .query_row(&query, [device_id], Device::from_row)
            .optional()
    }

    /// `user_id`'s devices, ordered by id.
    pub fn owned_by(&self, user_id: &str) -> rusqlite::Result<Vec<Device>> {
        let query = format!(
            "SELECT {} FROM devices WHERE owner = ?1 ORDER BY id",
            Device::COLUMNS
        );
        let connection = self.0.connection();
        let mut statement = connection.prepare_cached(&query)?;
        statement.query_map([user_id], Device::from_row)?.collect()
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
    let device_id = devices.register(&request.client_id, &request.name, PLATFORM_WINDOWS)?;
    tracing::info!(%device_id, "device registered");
    Ok(api::Ok(InitResponse { device_id }))
}

fn entry(device: Device, rooms: &Rooms) -> Entry {
    Entry {
        status: if rooms.is_online(&device.id) {
            Status::Connected
        } else {
            Status::Disconnected
        },
        alias: (!device.name.is_empty()).then_some(device.name),
        platform: device.platform,
        device_id: device.id,
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
    State(rooms): State<Rooms>,
    CallerDevice(caller): CallerDevice,
) -> Result<api::Ok<ListResponse>, ApiError> {
    let current_device = match caller {
        Some(id) => devices.get(&id)?.map(|device| entry(device, &rooms)),
        None => None,
    };
    let my_binded_devices = devices
        .owned_by(&user.id)?
        .into_iter()
        .map(|device| entry(device, &rooms))
        .collect();
    Ok(api::Ok(ListResponse {
        current_device,
        my_binded_devices,
        others_shared_devices: [],
    }))
}

/// `POST /api/v1/device/{device_id}/unbind`, with an empty body: removes one of the user's
/// devices from the account. Its host is logged out and dropped from signaling, so it has to
/// log in again to come back.
pub async fn unbind(
    user: User,
    Path(device_id): Path<String>,
    State(devices): State<Devices>,
    State(rooms): State<Rooms>,
) -> Result<api::Ok<Empty>, ApiError> {
    if !devices.unbind(&device_id, &user.id)? {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    rooms.kick_host(&device_id);
    tracing::info!(user_id = %user.id, %device_id, "device unbound");
    Ok(api::Ok(Empty {}))
}
