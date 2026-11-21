//! Devices: registration, ownership and the device list.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::api::address::ClientAddress;
use crate::api::files::{FileId, Files};
use crate::api::middleware::limit::Limits;
use crate::api::user::User;
use crate::api::wake::LanReports;
use crate::api::{self, ApiError, Body, Empty};
use crate::bus::ActiveParticipant;
use crate::room::RoomClient;
use crate::{Database, PublicUrl};

/// Identifies the calling installation once it has registered.
const DEVICE_ID_HEADER: &str = "x-param-device-id";
/// Carries the calling client's version string (e.g. `4.48.1` or `4.45.0.4318`).
const VERSION_HEADER: &str = "x-param-vn";

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

/// The calling client's reported version string (`x-param-vn`), if present and non-empty.
pub struct CallerVersion(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for CallerVersion {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        let version = parts
            .headers
            .get(VERSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        Ok(Self(version))
    }
}

/// What `device/windows/init` registers as, in UU's platform numbering.
const PLATFORM_WINDOWS: u8 = 1;
/// What `device/macos/init` registers as, in UU's platform numbering.
const PLATFORM_MACOS: u8 = 4;

fn is_desktop_platform(platform: u8) -> bool {
    matches!(platform, 1 | 4 | 5)
}

fn is_mobile_platform(platform: u8) -> bool {
    matches!(platform, 2 | 3)
}

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
    pub controllable: bool,
    pub version_name: String,
}

impl Device {
    const COLUMNS: &str = "id, name, platform, controllable, version_name";

    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            platform: row.get(2)?,
            controllable: row.get::<_, i64>(3)? != 0,
            version_name: row.get(4)?,
        })
    }
}

impl Devices {
    /// Registering the same installation again returns its existing device instead of a new
    /// one, renamed to `name` (and updating `platform`, `version_name`, and optionally
    /// `controllable`).
    fn register(
        &self,
        client_id: &str,
        name: &str,
        platform: u8,
        controllable: Option<bool>,
        version_name: Option<&str>,
    ) -> rusqlite::Result<Device> {
        let initial_controllable = i64::from(controllable.unwrap_or(true));
        let initial_version = version_name.unwrap_or("");
        let query = if controllable.is_some() {
            "INSERT INTO devices (id, client_id, name, platform, controllable, version_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (client_id) DO UPDATE SET
                 name = excluded.name,
                 platform = excluded.platform,
                 controllable = excluded.controllable,
                 version_name = CASE WHEN excluded.version_name = '' THEN devices.version_name ELSE excluded.version_name END
             RETURNING id, name, platform, controllable, version_name"
        } else {
            "INSERT INTO devices (id, client_id, name, platform, controllable, version_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (client_id) DO UPDATE SET
                 name = excluded.name,
                 platform = excluded.platform,
                 version_name = CASE WHEN excluded.version_name = '' THEN devices.version_name ELSE excluded.version_name END
             RETURNING id, name, platform, controllable, version_name"
        };
        self.0.connection().query_row(
            query,
            params![
                Uuid::new_v4().simple().to_string(),
                client_id,
                name,
                platform,
                initial_controllable,
                initial_version,
            ],
            Device::from_row,
        )
    }

    pub fn set_version_name(&self, device_id: &str, version_name: &str) -> rusqlite::Result<()> {
        if version_name.is_empty() {
            return Ok(());
        }
        self.0.connection().execute(
            "UPDATE devices SET version_name = ?2 WHERE id = ?1",
            [device_id, version_name],
        )?;
        Ok(())
    }

    fn set_controllable(
        &self,
        device_id: &str,
        user_id: &str,
        controllable: bool,
    ) -> rusqlite::Result<bool> {
        let changed = self.0.connection().execute(
            "UPDATE devices SET controllable = ?3
             WHERE id = ?1 AND (owner = ?2 OR owner IS NULL)",
            params![device_id, user_id, i64::from(controllable)],
        )?;
        Ok(changed > 0)
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

    /// Takes one of `user_id`'s devices off them, closes its host's sessions and forgets its
    /// LAN report, its wallpaper and the files it uploaded; `false` if it is not theirs.
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
        transaction.execute("DELETE FROM lan_reports WHERE device_id = ?1", [device_id])?;
        transaction.execute("DELETE FROM wallpapers WHERE device_id = ?1", [device_id])?;
        transaction.execute("DELETE FROM files WHERE device_id = ?1", [device_id])?;
        transaction.execute("DELETE FROM device_apps WHERE device_id = ?1", [device_id])?;
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

    fn get_apps(&self, device_id: &str) -> rusqlite::Result<Vec<Value>> {
        let raw: Option<String> = self
            .0
            .connection()
            .query_row(
                "SELECT apps_json FROM device_apps WHERE device_id = ?1",
                [device_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default())
    }

    fn set_apps(&self, device_id: &str, apps: &[Value]) -> rusqlite::Result<()> {
        let json = serde_json::to_string(apps).unwrap_or_else(|_| "[]".to_owned());
        self.0.connection().execute(
            "INSERT INTO device_apps (device_id, apps_json) VALUES (?1, ?2)
             ON CONFLICT (device_id) DO UPDATE SET apps_json = excluded.apps_json",
            [device_id, &json],
        )?;
        Ok(())
    }
}

/// Body of `POST /api/v1/device/{windows,macos}/init`. Hardware summary fields are ignored.
#[derive(Deserialize)]
pub struct InitRequest {
    client_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    controllable: Option<bool>,
}

async fn init_for_platform(
    services: DeviceInitService,
    address: Option<std::net::IpAddr>,
    version_name: Option<String>,
    request: InitRequest,
    platform: u8,
) -> Result<api::Ok<Entry>, ApiError> {
    if !services.limits.registration(address) {
        return Err(ApiError::TOO_MANY_REQUESTS);
    }
    if request.client_id.is_empty() {
        return Err(ApiError::INVALID_REQUEST);
    }
    let device = services.devices.register(
        &request.client_id,
        &request.name,
        platform,
        request.controllable,
        version_name.as_deref(),
    )?;
    tracing::info!(device_id = %device.id, platform, "device registered");
    let wallpaper = services.files.wallpaper_of(&device.id)?;
    let support_wol = services.lan_reports.has_report(&device.id)?;
    Ok(api::Ok(entry(
        device,
        wallpaper.as_ref(),
        false,
        support_wol,
        Vec::new(),
        &services.public_url,
    )))
}

#[derive(Clone)]
pub struct DeviceInitService {
    pub devices: Devices,
    pub files: Files,
    pub lan_reports: LanReports,
    pub public_url: PublicUrl,
    pub limits: Arc<Limits>,
}

pub async fn windows_init(
    State(services): State<DeviceInitService>,
    ClientAddress(address): ClientAddress,
    CallerVersion(version_name): CallerVersion,
    Body(request): Body<InitRequest>,
) -> Result<api::Ok<Entry>, ApiError> {
    init_for_platform(services, address, version_name, request, PLATFORM_WINDOWS).await
}

pub async fn macos_init(
    State(services): State<DeviceInitService>,
    ClientAddress(address): ClientAddress,
    CallerVersion(version_name): CallerVersion,
    Body(request): Body<InitRequest>,
) -> Result<api::Ok<Entry>, ApiError> {
    init_for_platform(services, address, version_name, request, PLATFORM_MACOS).await
}

#[derive(Deserialize)]
pub struct MacControllableRequest {
    controllable: bool,
}

/// `POST /api/v1/device/mac_controllable`: updates whether the calling Mac allows remote control.
pub async fn mac_controllable(
    user: User,
    CallerDevice(caller): CallerDevice,
    State(devices): State<Devices>,
    Body(request): Body<MacControllableRequest>,
) -> Result<api::Ok<Empty>, ApiError> {
    let device_id = caller.ok_or(ApiError::UNKNOWN_DEVICE)?;
    if !devices.set_controllable(&device_id, &user.id, request.controllable)? {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    Ok(api::Ok(Empty {}))
}

#[derive(Serialize)]
pub struct ParticipantEntry {
    client_id: String,
    device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    platform: u8,
    user_join_type: u8,
    controlled_time: u64,
    app_flag: Option<Value>,
}

fn entry(
    device: Device,
    wallpaper: Option<&FileId>,
    online: bool,
    support_wol: bool,
    participants_info: Vec<ParticipantEntry>,
    public_url: &PublicUrl,
) -> Entry {
    let publisher_availability_status = if is_desktop_platform(device.platform) {
        Some(if online { "ready" } else { "offline" })
    } else {
        None
    };
    Entry {
        wallpaper_url: wallpaper.map(|file| file.url(public_url)),
        status: if online {
            Status::Connected
        } else {
            Status::Disconnected
        },
        alias: (!device.name.is_empty()).then_some(device.name),
        platform: device.platform,
        device_id: device.id,
        participants_info,
        version_name: device.version_name,
        support_wol,
        controllable: device.controllable,
        publisher_availability_status,
        controlled_support: is_desktop_platform(device.platform),
        update_started_at: 0,
    }
}

#[derive(Serialize)]
pub struct Entry {
    device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    platform: u8,
    status: Status,
    participants_info: Vec<ParticipantEntry>,
    /// The image the device's card shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    wallpaper_url: Option<String>,
    version_name: String,
    support_wol: bool,
    controllable: bool,
    publisher_availability_status: Option<&'static str>,
    controlled_support: bool,
    update_started_at: u64,
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

#[derive(Serialize)]
pub struct GroupsResponse {
    current_device_id: Option<String>,
    mobile_devices: Vec<Entry>,
    desktop_devices: Vec<Entry>,
    tv_devices: Vec<Entry>,
}

struct DeviceSnapshot {
    current_device: Option<Entry>,
    owned: Vec<Entry>,
}

fn build_participants(
    devices: &Devices,
    cache: &mut HashMap<String, Option<Device>>,
    raw: Vec<ActiveParticipant>,
) -> Vec<ParticipantEntry> {
    raw.into_iter()
        .map(|participant| {
            let (device_id, alias, platform) = match participant.from_device {
                Some(ref dev_id) => {
                    let info = cache
                        .entry(dev_id.clone())
                        .or_insert_with(|| devices.get(dev_id).ok().flatten());
                    match info {
                        Some(d) => (
                            d.id.clone(),
                            (!d.name.is_empty()).then(|| d.name.clone()),
                            d.platform,
                        ),
                        None => (dev_id.clone(), None, 0),
                    }
                }
                None => (participant.client_id.clone(), None, 0),
            };
            ParticipantEntry {
                client_id: participant.client_id,
                device_id,
                alias,
                platform,
                user_join_type: 1,
                controlled_time: participant.controlled_secs,
                app_flag: None,
            }
        })
        .collect()
}

async fn snapshot_for_user(
    user: &User,
    devices: &Devices,
    rooms: &RoomClient,
    files: &Files,
    lan_reports: &LanReports,
    public_url: &PublicUrl,
    caller: Option<&str>,
) -> Result<DeviceSnapshot, ApiError> {
    let caller_device = match caller {
        Some(id) => devices.get(id)?,
        None => None,
    };
    let owned = devices.owned_by(&user.id)?;
    let mut query_ids: Vec<String> = owned.iter().map(|d| d.id.clone()).collect();
    if let Some(device) = &caller_device
        && !query_ids.contains(&device.id)
    {
        query_ids.push(device.id.clone());
    }
    let online = rooms.online_devices(query_ids.clone()).await;
    let mut participants_by_device = rooms.desktop_participants(query_ids).await;
    let reported: HashSet<String> = lan_reports.reported_devices(&user.id)?;
    let mut device_cache: HashMap<String, Option<Device>> = HashMap::new();

    let current_device = match caller_device {
        Some(device) => {
            let wallpaper = files.wallpaper_of(&device.id)?;
            let is_online = online.contains(&device.id);
            let support_wol =
                reported.contains(&device.id) || lan_reports.has_report(&device.id)?;
            let raw_parts = participants_by_device
                .get(&device.id)
                .cloned()
                .unwrap_or_default();
            let participants = build_participants(devices, &mut device_cache, raw_parts);
            Some(entry(
                device,
                wallpaper.as_ref(),
                is_online,
                support_wol,
                participants,
                public_url,
            ))
        }
        None => None,
    };

    let wallpapers = files.wallpapers_of_user(&user.id)?;
    let owned_entries = owned
        .into_iter()
        .map(|device| {
            let wallpaper = wallpapers.get(&device.id);
            let is_online = online.contains(&device.id);
            let support_wol = reported.contains(&device.id);
            let raw_parts = participants_by_device
                .remove(&device.id)
                .unwrap_or_default();
            let participants = build_participants(devices, &mut device_cache, raw_parts);
            entry(
                device,
                wallpaper,
                is_online,
                support_wol,
                participants,
                public_url,
            )
        })
        .collect();

    Ok(DeviceSnapshot {
        current_device,
        owned: owned_entries,
    })
}

/// `GET /api/v1/device/list`: the user's devices.
pub async fn list(
    user: User,
    State(devices): State<Devices>,
    State(rooms): State<RoomClient>,
    State(files): State<Files>,
    State(lan_reports): State<LanReports>,
    State(public_url): State<PublicUrl>,
    CallerDevice(caller): CallerDevice,
) -> Result<api::Ok<ListResponse>, ApiError> {
    let snapshot = snapshot_for_user(
        &user,
        &devices,
        &rooms,
        &files,
        &lan_reports,
        &public_url,
        caller.as_deref(),
    )
    .await?;
    Ok(api::Ok(ListResponse {
        current_device: snapshot.current_device,
        my_binded_devices: snapshot.owned,
        others_shared_devices: [],
    }))
}

/// `GET /api/v1/device/groups/of/my`: the user's devices grouped by form factor.
pub async fn groups(
    user: User,
    State(devices): State<Devices>,
    State(rooms): State<RoomClient>,
    State(files): State<Files>,
    State(lan_reports): State<LanReports>,
    State(public_url): State<PublicUrl>,
    CallerDevice(caller): CallerDevice,
) -> Result<api::Ok<GroupsResponse>, ApiError> {
    let snapshot = snapshot_for_user(
        &user,
        &devices,
        &rooms,
        &files,
        &lan_reports,
        &public_url,
        caller.as_deref(),
    )
    .await?;
    let mut mobile_devices = Vec::new();
    let mut desktop_devices = Vec::new();
    let mut tv_devices = Vec::new();
    for item in snapshot.owned {
        if is_mobile_platform(item.platform) {
            mobile_devices.push(item);
        } else if is_desktop_platform(item.platform) {
            desktop_devices.push(item);
        } else {
            tv_devices.push(item);
        }
    }
    Ok(api::Ok(GroupsResponse {
        current_device_id: snapshot.current_device.map(|d| d.device_id),
        mobile_devices,
        desktop_devices,
        tv_devices,
    }))
}

#[derive(Serialize)]
pub struct PreferencesResponse {
    device_preferences: DevicePreferencesBody,
}

#[derive(Serialize)]
struct DevicePreferencesBody {
    device_id: String,
    tools: ToolsPreference,
}

#[derive(Serialize)]
struct ToolsPreference {
    sequence: [&'static str; 7],
}

/// `GET /api/v1/device/{device_id}/preferences`: toolbar order for one of the user's devices.
pub async fn preferences(
    user: User,
    Path(device_id): Path<String>,
    State(devices): State<Devices>,
) -> Result<api::Ok<PreferencesResponse>, ApiError> {
    if devices.platform_of_owned(&device_id, &user.id)?.is_none() {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    Ok(api::Ok(PreferencesResponse {
        device_preferences: DevicePreferencesBody {
            device_id,
            tools: ToolsPreference {
                sequence: [
                    "file_transfer",
                    "view_mode",
                    "remote_connection_cli",
                    "port_mapping",
                    "wake_on_lan",
                    "device_reboot",
                    "device_shutdown",
                ],
            },
        },
    }))
}

#[derive(Deserialize, Serialize)]
pub struct AppListPayload {
    device_app_list: Vec<Value>,
}

/// `GET /api/v1/app/list/of/device/{device_id}`: registered app list of one of the user's
/// devices.
pub async fn app_list_of_device(
    user: User,
    Path(device_id): Path<String>,
    State(devices): State<Devices>,
) -> Result<api::Ok<AppListPayload>, ApiError> {
    if devices.platform_of_owned(&device_id, &user.id)?.is_none() {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    let device_app_list = devices.get_apps(&device_id)?;
    Ok(api::Ok(AppListPayload { device_app_list }))
}

/// `POST /api/v1/app/list/of/current_device`: updates the calling host's reported app list.
pub async fn update_current_app_list(
    user: User,
    CallerDevice(caller): CallerDevice,
    State(devices): State<Devices>,
    Body(payload): Body<AppListPayload>,
) -> Result<api::Ok<AppListPayload>, ApiError> {
    let device_id = caller.ok_or(ApiError::UNKNOWN_DEVICE)?;
    if devices.platform_of_owned(&device_id, &user.id)?.is_none() {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    devices.set_apps(&device_id, &payload.device_app_list)?;
    Ok(api::Ok(payload))
}

/// `POST /api/v1/device/{device_id}/unbind`, with an empty body: removes one of the user's
/// devices from the account. Its host is logged out and dropped from signaling, so it has to
/// log in again to come back.
pub async fn unbind(
    user: User,
    Path(device_id): Path<String>,
    State(devices): State<Devices>,
    State(rooms): State<RoomClient>,
) -> Result<api::Ok<Empty>, ApiError> {
    if !devices.unbind(&device_id, &user.id)? {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    rooms.kick_host(&device_id).await;
    tracing::info!(user_id = %user.id, %device_id, "device unbound");
    Ok(api::Ok(Empty {}))
}
