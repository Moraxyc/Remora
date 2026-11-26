//! Guest remote-assistance identities, share info, live challenge coordination, and
//! saved/shared device records.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{FromRef, FromRequestParts, Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use remora_proto::control::{
    AssistDeviceRecord, AssistDeviceRecordPage, AssistFavoritesRequest, AssistRoomConfig,
    CancelRemoteAssistRequest, DeviceShareInfoResponse, GuestCreateResponse,
    JoinShareByCodeRequest, JoinShareByConfirmationRequest, RoomCreateRequest,
    ShareConfirmationRequest, ShareControlMode, ShareControlModeRequest, ShareControlModeResponse,
    UploadControlModeRequest, UploadShareSignRequest, verify_share_code_digest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use crate::Database;
use crate::api::address::ClientAddress;
use crate::api::device::{CallerDevice, CallerVersion, Device, Devices, connect_id_for};
use crate::api::middleware::limit::Limits;
use crate::api::room::Tickets;
use crate::api::user::User;
use crate::api::{self, ApiError, Body, Empty, OptionalBody};
use crate::contracts::{GuestToken, Room, TokenKey, now, random};
use crate::room::RoomClient;

const GUEST_ID_HEADER: &str = "x-param-guest-id";

const WS_CONNECT_TIMEOUT_MS: u32 = 16_000;
const MAX_RECONNECT_DELTA_SECS: u32 = 300;
const STREAMER_RETRY_DELTA_MS: u32 = 1_000;

/// How long the cloud waits for the online host to reply to `get_control_mode` or
/// `remote_control` pushes.
const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(8);
/// Shorter grace period when querying `get_control_mode` on macOS hosts (whose official binary
/// omits `upload_control_mode`).
const MACOS_CONTROL_MODE_GRACE: Duration = Duration::from_millis(800);
/// How long a `remote_control_by_confirmation` prompt stays alive for polling.
const CONFIRMATION_LIFETIME: Duration = Duration::from_secs(120);

/// SQLite-backed store for guest identities, share identifiers, and user shared-device lists.
#[derive(Debug, Clone)]
pub struct Shares(Database);

impl From<Database> for Shares {
    fn from(database: Database) -> Self {
        Self(database)
    }
}

pub struct ShareTarget {
    pub device_id: String,
    pub owner: Option<String>,
    pub platform: u8,
    pub name: String,
    pub version_name: String,
    pub controllable: bool,
    pub active: bool,
}

impl Shares {
    /// Creates a guest identity bound to a registered `device_id`.
    pub fn create_guest(&self, device_id: &str) -> rusqlite::Result<String> {
        let guest_id = hex::encode(random::<16>());
        let created: i64 = now().try_into().unwrap_or(i64::MAX);
        self.0.connection().execute(
            "INSERT INTO guests (id, device_id, room_created, created) VALUES (?1, ?2, 0, ?3)",
            rusqlite::params![&guest_id, device_id, created],
        )?;
        Ok(guest_id)
    }

    pub fn guest_exists(&self, guest_id: &str, device_id: &str) -> rusqlite::Result<bool> {
        let found: Option<i64> = self
            .0
            .connection()
            .query_row(
                "SELECT 1 FROM guests WHERE id = ?1 AND device_id = ?2",
                [guest_id, device_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn mark_guest_room_created(&self, guest_id: &str) -> rusqlite::Result<()> {
        self.0.connection().execute(
            "UPDATE guests SET room_created = 1 WHERE id = ?1",
            [guest_id],
        )?;
        Ok(())
    }

    pub fn has_guest_room(&self, guest_id: &str) -> rusqlite::Result<bool> {
        let room_created: Option<i64> = self
            .0
            .connection()
            .query_row(
                "SELECT room_created FROM guests WHERE id = ?1",
                [guest_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(room_created == Some(1))
    }

    /// Ensures a share row exists for `device_id`, optionally marks it active, and returns its
    /// `connect_id`.
    pub fn ensure_share(
        &self,
        device_id: &str,
        guest_id: Option<&str>,
        activate: bool,
    ) -> rusqlite::Result<String> {
        let mut connection = self.0.connection();
        let tx = connection.transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT connect_id FROM device_shares WHERE device_id = ?1",
                [device_id],
                |row| row.get(0),
            )
            .optional()?;
        let updated: i64 = now().try_into().unwrap_or(i64::MAX);
        let connect_id = match existing {
            Some(id) => {
                if activate {
                    tx.execute(
                        "UPDATE device_shares
                         SET active = 1,
                             guest_id = COALESCE(?2, guest_id),
                             updated = ?3
                         WHERE device_id = ?1",
                        rusqlite::params![device_id, guest_id, updated],
                    )?;
                }
                id
            }
            None => {
                let candidate = unique_connect_id(&tx, Some(device_id))?;
                tx.execute(
                    "INSERT INTO device_shares (device_id, connect_id, guest_id, active, updated)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        device_id,
                        &candidate,
                        guest_id,
                        i64::from(activate),
                        updated
                    ],
                )?;
                candidate
            }
        };
        tx.commit()?;
        Ok(connect_id)
    }

    /// Replaces `device_id`'s `connect_id` with a fresh random 9-digit identifier.
    pub fn refresh_share_id(
        &self,
        device_id: &str,
        guest_id: Option<&str>,
    ) -> rusqlite::Result<String> {
        let mut connection = self.0.connection();
        let tx = connection.transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT connect_id FROM device_shares WHERE device_id = ?1",
                [device_id],
                |row| row.get(0),
            )
            .optional()?;
        let mut connect_id = unique_connect_id(&tx, None)?;
        if current.as_deref() == Some(connect_id.as_str()) {
            connect_id = unique_connect_id(&tx, None)?;
        }
        let updated: i64 = now().try_into().unwrap_or(i64::MAX);
        tx.execute(
            "INSERT INTO device_shares (device_id, connect_id, guest_id, active, updated)
             VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT (device_id) DO UPDATE SET
                 connect_id = excluded.connect_id,
                 guest_id = COALESCE(excluded.guest_id, device_shares.guest_id),
                 active = 1,
                 updated = excluded.updated",
            rusqlite::params![device_id, &connect_id, guest_id, updated],
        )?;
        tx.commit()?;
        Ok(connect_id)
    }

    pub fn cancel_share(&self, device_id: &str) -> rusqlite::Result<()> {
        let updated: i64 = now().try_into().unwrap_or(i64::MAX);
        self.0.connection().execute(
            "UPDATE device_shares SET active = 0, updated = ?2 WHERE device_id = ?1",
            rusqlite::params![device_id, updated],
        )?;
        Ok(())
    }

    pub fn is_share_active(&self, device_id: &str) -> rusqlite::Result<bool> {
        let active: Option<i64> = self
            .0
            .connection()
            .query_row(
                "SELECT active FROM device_shares WHERE device_id = ?1",
                [device_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(active == Some(1))
    }

    pub fn resolve_by_connect_id(&self, connect_id: &str) -> rusqlite::Result<Option<ShareTarget>> {
        self.0
            .connection()
            .query_row(
                "SELECT d.id, d.owner, d.platform, d.name, d.version_name, d.controllable, s.active
                 FROM device_shares s
                 JOIN devices d ON d.id = s.device_id
                 WHERE s.connect_id = ?1",
                [connect_id],
                |row| {
                    Ok(ShareTarget {
                        device_id: row.get(0)?,
                        owner: row.get(1)?,
                        platform: row.get(2)?,
                        name: row.get(3)?,
                        version_name: row.get(4)?,
                        controllable: row.get::<_, i64>(5)? != 0,
                        active: row.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .optional()
    }

    pub fn record_shared_connection(&self, user_id: &str, device_id: &str) -> rusqlite::Result<()> {
        let ts: i64 = now().try_into().unwrap_or(i64::MAX);
        self.0.connection().execute(
            "INSERT INTO shared_devices (user_id, device_id, last_connected_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (user_id, device_id) DO UPDATE SET
                 last_connected_at = excluded.last_connected_at",
            rusqlite::params![user_id, device_id, ts],
        )?;
        Ok(())
    }

    pub fn shared_devices_for(&self, user_id: &str) -> rusqlite::Result<Vec<Device>> {
        const QUERY: &str = "SELECT d.id, d.name, d.platform, d.controllable, d.version_name
             FROM shared_devices sd
             JOIN devices d ON d.id = sd.device_id
             WHERE sd.user_id = ?1 AND (d.owner IS NULL OR d.owner != ?1)
             ORDER BY sd.last_connected_at DESC, d.id";
        let connection = self.0.connection();
        let mut stmt = connection.prepare_cached(QUERY)?;
        stmt.query_map([user_id], |row| {
            Ok(Device {
                id: row.get(0)?,
                name: row.get(1)?,
                platform: row.get(2)?,
                controllable: row.get::<_, i64>(3)? != 0,
                version_name: row.get(4)?,
            })
        })?
        .collect()
    }

    pub fn list_saved(
        &self,
        user_id: &str,
        kind: SavedKind,
        limit: usize,
        offset: usize,
    ) -> rusqlite::Result<AssistDeviceRecordPage> {
        let filter = match kind {
            SavedKind::All => "",
            SavedKind::Favorites => "AND sd.is_favorite = 1",
            SavedKind::Recent => "AND sd.last_connected_at > 0",
        };
        let order = match kind {
            SavedKind::Favorites => "sd.favorited_at DESC, sd.device_id",
            SavedKind::Recent | SavedKind::All => {
                "sd.last_connected_at DESC, sd.favorited_at DESC, sd.device_id"
            }
        };
        let sql = format!(
            "SELECT sd.device_id, COALESCE(ds.connect_id, ''), d.platform, sd.remark,
                    sd.is_favorite, sd.last_connected_at, sd.favorited_at
             FROM shared_devices sd
             JOIN devices d ON d.id = sd.device_id
             LEFT JOIN device_shares ds ON ds.device_id = sd.device_id
             WHERE sd.user_id = ?1 AND (d.owner IS NULL OR d.owner != ?1) {filter}
             ORDER BY {order}
             LIMIT ?2 OFFSET ?3"
        );
        let connection = self.0.connection();
        let mut stmt = connection.prepare(&sql)?;
        let fetch_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(101);
        let offset_i64 = i64::try_from(offset).unwrap_or(0);
        let mut rows: Vec<AssistDeviceRecord> = stmt
            .query_map(rusqlite::params![user_id, fetch_limit, offset_i64], |row| {
                let device_id: String = row.get(0)?;
                let mut connect_id: String = row.get(1)?;
                if connect_id.is_empty() {
                    connect_id = connect_id_for(&device_id);
                }
                let last_connected: i64 = row.get(5)?;
                let favorited_at: i64 = row.get(6)?;
                Ok(AssistDeviceRecord {
                    publisher_device_id: device_id,
                    connect_id,
                    platform: row.get(2)?,
                    remark: row.get(3)?,
                    is_favorite: row.get::<_, i64>(4)? != 0,
                    last_connected_at: u64::try_from(last_connected).unwrap_or(0),
                    favorited_at: u64::try_from(favorited_at).unwrap_or(0),
                    extra: Map::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next = if rows.len() > limit {
            rows.truncate(limit);
            Some((offset + limit).to_string())
        } else {
            None
        };
        Ok(AssistDeviceRecordPage { saved: rows, next })
    }

    pub fn save_favorites(
        &self,
        user_id: &str,
        request: &AssistFavoritesRequest,
    ) -> Result<(), ApiError> {
        let mut connection = self.0.connection();
        let tx = connection.transaction()?;
        for item in &request.favorites {
            let target: Option<(String, Option<String>)> = tx
                .query_row(
                    "SELECT d.id, d.owner
                     FROM device_shares s
                     JOIN devices d ON d.id = s.device_id
                     WHERE s.connect_id = ?1",
                    [&item.connect_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((device_id, owner)) = target else {
                if request.skip_invalid {
                    continue;
                }
                return Err(ApiError::UNKNOWN_DEVICE);
            };
            if owner.as_deref() == Some(user_id) {
                if request.skip_invalid {
                    continue;
                }
                return Err(ApiError::SAME_ACCOUNT_SHARE_FAVORITE);
            }
            let favorited_at: i64 = if item.favorited_at > 0 {
                i64::try_from(item.favorited_at).unwrap_or(i64::MAX)
            } else {
                now().try_into().unwrap_or(i64::MAX)
            };
            tx.execute(
                "INSERT INTO shared_devices (user_id, device_id, remark, is_favorite, favorited_at)
                 VALUES (?1, ?2, ?3, 1, ?4)
                 ON CONFLICT (user_id, device_id) DO UPDATE SET
                     remark = CASE WHEN excluded.remark = '' THEN shared_devices.remark ELSE excluded.remark END,
                     is_favorite = 1,
                     favorited_at = excluded.favorited_at",
                rusqlite::params![user_id, &device_id, &item.remark, favorited_at],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_saved(
        &self,
        user_id: &str,
        kind: SavedKind,
        publisher_device_id: &str,
    ) -> rusqlite::Result<bool> {
        let mut connection = self.0.connection();
        let tx = connection.transaction()?;
        let changed = match kind {
            SavedKind::All => tx.execute(
                "DELETE FROM shared_devices WHERE user_id = ?1 AND device_id = ?2",
                [user_id, publisher_device_id],
            )?,
            SavedKind::Favorites => {
                let n = tx.execute(
                    "UPDATE shared_devices
                     SET is_favorite = 0, favorited_at = 0
                     WHERE user_id = ?1 AND device_id = ?2",
                    [user_id, publisher_device_id],
                )?;
                tx.execute(
                    "DELETE FROM shared_devices
                     WHERE user_id = ?1 AND device_id = ?2
                       AND is_favorite = 0 AND last_connected_at = 0",
                    [user_id, publisher_device_id],
                )?;
                n
            }
            SavedKind::Recent => {
                let n = tx.execute(
                    "UPDATE shared_devices
                     SET last_connected_at = 0
                     WHERE user_id = ?1 AND device_id = ?2",
                    [user_id, publisher_device_id],
                )?;
                tx.execute(
                    "DELETE FROM shared_devices
                     WHERE user_id = ?1 AND device_id = ?2
                       AND is_favorite = 0 AND last_connected_at = 0",
                    [user_id, publisher_device_id],
                )?;
                n
            }
        };
        tx.commit()?;
        Ok(changed > 0)
    }

    pub fn unbind_shared(
        &self,
        user_id: &str,
        id_or_connect_id: &str,
    ) -> rusqlite::Result<Option<String>> {
        let connection = self.0.connection();
        if let Some(device_id) = connection
            .query_row(
                "DELETE FROM shared_devices
                 WHERE user_id = ?1 AND device_id = ?2
                 RETURNING device_id",
                [user_id, id_or_connect_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(Some(device_id));
        }
        connection
            .query_row(
                "DELETE FROM shared_devices
                 WHERE user_id = ?1
                   AND device_id = (SELECT device_id FROM device_shares WHERE connect_id = ?2)
                 RETURNING device_id",
                [user_id, id_or_connect_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
    }
}

use rusqlite::OptionalExtension;

fn random_connect_id() -> String {
    let bytes = random::<8>();
    let n = u64::from_le_bytes(bytes);
    format!("{:09}", 100_000_000 + (n % 900_000_000))
}

fn unique_connect_id(
    tx: &rusqlite::Transaction<'_>,
    deterministic_seed: Option<&str>,
) -> rusqlite::Result<String> {
    if let Some(seed) = deterministic_seed {
        let candidate = connect_id_for(seed);
        let taken: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM device_shares WHERE connect_id = ?1",
                [&candidate],
                |row| row.get(0),
            )
            .optional()?;
        if taken.is_none() {
            return Ok(candidate);
        }
    }
    loop {
        let candidate = random_connect_id();
        let taken: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM device_shares WHERE connect_id = ?1",
                [&candidate],
                |row| row.get(0),
            )
            .optional()?;
        if taken.is_none() {
            return Ok(candidate);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedKind {
    All,
    Favorites,
    Recent,
}

/// Extracted guest identity from `Authorization: Bearer <Guest JWT>` and optional
/// `X-Param-guest-id`.
pub struct Guest {
    pub id: String,
    pub device_id: String,
}

impl<S> FromRequestParts<S> for Guest
where
    Arc<TokenKey>: FromRef<S>,
    Shares: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|token| Arc::<TokenKey>::from_ref(state).verify_guest(token))
            .ok_or(ApiError::TOKEN_INVALID)?;
        if let Some(header_guest) = parts
            .headers
            .get(GUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            && header_guest != token.guest_id
        {
            return Err(ApiError::TOKEN_INVALID);
        }
        if !Shares::from_ref(state).guest_exists(&token.guest_id, &token.device_id)? {
            return Err(ApiError::TOKEN_INVALID);
        }
        Ok(Self {
            id: token.guest_id,
            device_id: token.device_id,
        })
    }
}

/// In-memory coordinator for live `get_control_mode`, `remote_control` (code sign), and
/// `remote_control_by_confirmation` challenges.
#[derive(Debug, Clone, Default)]
pub struct ShareChallenges {
    inner: Arc<Mutex<ChallengeState>>,
}

#[derive(Debug, Default)]
struct ChallengeState {
    control_modes: HashMap<String, ControlModeWaiter>,
    signs: HashMap<String, SignWaiter>,
    confirmations: HashMap<String, PendingConfirmation>,
}

#[derive(Debug)]
struct ControlModeWaiter {
    device_id: String,
    tx: oneshot::Sender<UploadControlModeRequest>,
}

#[derive(Debug)]
struct SignWaiter {
    device_id: String,
    tx: oneshot::Sender<UploadShareSignRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationStatus {
    Pending,
    Approved,
    Denied,
}

#[derive(Debug)]
struct PendingConfirmation {
    user_id: String,
    connect_id: String,
    device_id: String,
    status: ConfirmationStatus,
    created: Instant,
}

impl PendingConfirmation {
    fn expired(&self) -> bool {
        self.created.elapsed() >= CONFIRMATION_LIFETIME
    }
}

impl ShareChallenges {
    pub fn register_control_mode(
        &self,
        device_id: &str,
    ) -> (String, oneshot::Receiver<UploadControlModeRequest>) {
        let control_id = hex::encode(random::<16>());
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .expect("share challenges lock poisoned")
            .control_modes
            .insert(
                control_id.clone(),
                ControlModeWaiter {
                    device_id: device_id.to_owned(),
                    tx,
                },
            );
        (control_id, rx)
    }

    pub fn remove_control_mode(&self, control_id: &str) {
        self.inner
            .lock()
            .expect("share challenges lock poisoned")
            .control_modes
            .remove(control_id);
    }

    pub fn complete_control_mode(&self, device_id: &str, req: UploadControlModeRequest) -> bool {
        let waiter = {
            let mut state = self.inner.lock().expect("share challenges lock poisoned");
            match state.control_modes.get(&req.control_id) {
                Some(w) if w.device_id == device_id => state.control_modes.remove(&req.control_id),
                _ => return false,
            }
        };
        if let Some(waiter) = waiter {
            let _ = waiter.tx.send(req);
            true
        } else {
            false
        }
    }

    pub fn register_sign(
        &self,
        device_id: &str,
    ) -> (String, String, oneshot::Receiver<UploadShareSignRequest>) {
        let control_id = hex::encode(random::<16>());
        let salt = hex::encode(random::<16>());
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .expect("share challenges lock poisoned")
            .signs
            .insert(
                control_id.clone(),
                SignWaiter {
                    device_id: device_id.to_owned(),
                    tx,
                },
            );
        (control_id, salt, rx)
    }

    pub fn remove_sign(&self, control_id: &str) {
        self.inner
            .lock()
            .expect("share challenges lock poisoned")
            .signs
            .remove(control_id);
    }

    pub fn complete_sign(&self, device_id: &str, req: UploadShareSignRequest) -> bool {
        let waiter = {
            let mut state = self.inner.lock().expect("share challenges lock poisoned");
            match state.signs.get(&req.control_id) {
                Some(w) if w.device_id == device_id => state.signs.remove(&req.control_id),
                _ => return false,
            }
        };
        if let Some(waiter) = waiter {
            let _ = waiter.tx.send(req);
            true
        } else {
            false
        }
    }

    /// Looks up or creates a confirmation prompt for `(user_id, connect_id, device_id)`.
    /// Returns `(control_id, status, newly_created)`.
    pub fn poll_or_start_confirmation(
        &self,
        user_id: &str,
        connect_id: &str,
        device_id: &str,
        requested_control_id: Option<&str>,
    ) -> (String, ConfirmationStatus, bool) {
        let mut state = self.inner.lock().expect("share challenges lock poisoned");
        state.confirmations.retain(|_, c| !c.expired());

        if let Some(cid) = requested_control_id
            && let Some(entry) = state.confirmations.get(cid)
            && entry.user_id == user_id
            && entry.connect_id == connect_id
            && entry.device_id == device_id
        {
            let status = entry.status;
            if status != ConfirmationStatus::Pending {
                state.confirmations.remove(cid);
            }
            return (cid.to_owned(), status, false);
        }

        if requested_control_id.is_none()
            && let Some((cid, status)) = state
                .confirmations
                .iter()
                .find(|(_, c)| {
                    c.user_id == user_id && c.connect_id == connect_id && c.device_id == device_id
                })
                .map(|(cid, c)| (cid.clone(), c.status))
        {
            if status != ConfirmationStatus::Pending {
                state.confirmations.remove(&cid);
            }
            return (cid, status, false);
        }

        let control_id = hex::encode(random::<16>());
        state.confirmations.insert(
            control_id.clone(),
            PendingConfirmation {
                user_id: user_id.to_owned(),
                connect_id: connect_id.to_owned(),
                device_id: device_id.to_owned(),
                status: ConfirmationStatus::Pending,
                created: Instant::now(),
            },
        );
        (control_id, ConfirmationStatus::Pending, true)
    }

    pub fn decide_confirmation(&self, device_id: &str, control_id: &str, allow: bool) -> bool {
        let mut state = self.inner.lock().expect("share challenges lock poisoned");
        state.confirmations.retain(|_, c| !c.expired());
        let Some(entry) = state.confirmations.get_mut(control_id) else {
            return false;
        };
        if entry.device_id != device_id {
            return false;
        }
        entry.status = if allow {
            ConfirmationStatus::Approved
        } else {
            ConfirmationStatus::Denied
        };
        true
    }

    pub fn cancel_for_viewer(
        &self,
        user_id: &str,
        connect_id: &str,
        control_id: Option<&str>,
    ) -> Vec<(String, String)> {
        let mut state = self.inner.lock().expect("share challenges lock poisoned");
        let mut cancelled = Vec::new();
        state.confirmations.retain(|cid, entry| {
            let matches = entry.user_id == user_id
                && entry.connect_id == connect_id
                && control_id.is_none_or(|wanted| wanted == cid);
            if matches {
                cancelled.push((entry.device_id.clone(), cid.clone()));
                false
            } else {
                !entry.expired()
            }
        });
        cancelled
    }

    pub fn cancel_for_device(&self, device_id: &str) {
        let mut state = self.inner.lock().expect("share challenges lock poisoned");
        state.control_modes.retain(|_, w| w.device_id != device_id);
        state.signs.retain(|_, w| w.device_id != device_id);
        state.confirmations.retain(|_, c| c.device_id != device_id);
    }
}

#[derive(Clone)]
pub struct ShareService {
    pub shares: Shares,
    pub devices: Devices,
    pub rooms: RoomClient,
    pub tickets: Tickets,
    pub challenges: ShareChallenges,
    pub limits: Arc<Limits>,
}

/// `POST /api/v1/guest/create`: issues a guest identity for an already-registered device.
pub async fn guest_create(
    State(service): State<ShareService>,
    ClientAddress(address): ClientAddress,
    CallerDevice(caller): CallerDevice,
    OptionalBody(_body): OptionalBody<Value>,
) -> Result<api::Ok<GuestCreateResponse>, ApiError> {
    if !service.limits.registration(address) {
        return Err(ApiError::TOO_MANY_REQUESTS);
    }
    let device_id = caller.ok_or(ApiError::INVALID_REQUEST)?;
    if service.devices.get(&device_id)?.is_none() {
        return Err(ApiError::INVALID_REQUEST);
    }
    let guest_id = service.shares.create_guest(&device_id)?;
    let token = service.tickets.tokens.issue_guest(&GuestToken {
        guest_id: guest_id.clone(),
        device_id: device_id.clone(),
    });
    tracing::info!(%device_id, %guest_id, "guest identity created");
    Ok(api::Ok(GuestCreateResponse {
        guest_id,
        token,
        extra: Map::new(),
    }))
}

#[derive(Serialize)]
pub struct GuestRoomResponse {
    signaling_server: String,
    signaling_list: [String; 1],
    sni_required_signaling_list: Vec<String>,
    token: String,
    ws_connect_timeout_ms: u32,
    max_reconnect_delta: u32,
    streamer_retry_delta_ms: u32,
    international_connect: bool,
}

/// `POST /api/v1/guest/room/create`: creates/renews a signaling room for a guest host.
pub async fn guest_room_create(
    guest: Guest,
    State(service): State<ShareService>,
    CallerVersion(version_name): CallerVersion,
    OptionalBody(_body): OptionalBody<RoomCreateRequest>,
) -> Result<api::Ok<GuestRoomResponse>, ApiError> {
    if let Some(version_name) = version_name.as_deref() {
        service
            .devices
            .set_version_name(&guest.device_id, version_name)?;
    }
    service.shares.mark_guest_room_created(&guest.id)?;
    let (token, signaling_server) = service
        .tickets
        .issue(Room::guest_host(guest.id.clone(), guest.device_id.clone()));
    let sni_required_signaling_list = if service.tickets.public_url.requires_sni() {
        vec![signaling_server.clone()]
    } else {
        Vec::new()
    };
    tracing::info!(guest_id = %guest.id, device_id = %guest.device_id, "guest room created");
    Ok(api::Ok(GuestRoomResponse {
        signaling_list: [signaling_server.clone()],
        signaling_server,
        sni_required_signaling_list,
        token,
        ws_connect_timeout_ms: WS_CONNECT_TIMEOUT_MS,
        max_reconnect_delta: MAX_RECONNECT_DELTA_SECS,
        streamer_retry_delta_ms: STREAMER_RETRY_DELTA_MS,
        international_connect: false,
    }))
}

/// `POST /api/v1/guest/share/info`: publishes the guest host's share ID once its room exists.
pub async fn guest_share_info(
    guest: Guest,
    State(service): State<ShareService>,
    OptionalBody(_body): OptionalBody<Value>,
) -> Result<api::Ok<DeviceShareInfoResponse>, ApiError> {
    if !service.shares.has_guest_room(&guest.id)? {
        return Err(ApiError::GUEST_ROOM_REQUIRED);
    }
    let device = service
        .devices
        .get(&guest.device_id)?
        .ok_or(ApiError::UNKNOWN_DEVICE)?;
    let connect_id = service
        .shares
        .ensure_share(&guest.device_id, Some(&guest.id), true)?;
    let alias = if device.name.is_empty() {
        device.id
    } else {
        device.name
    };
    Ok(api::Ok(DeviceShareInfoResponse {
        connect_id,
        connect_code: String::new(),
        alias,
        extra: Map::new(),
    }))
}

/// `POST /api/v1/guest/share/cancel`.
pub async fn guest_share_cancel(
    guest: Guest,
    State(service): State<ShareService>,
    OptionalBody(_body): OptionalBody<Value>,
) -> Result<api::Ok<Empty>, ApiError> {
    service.shares.cancel_share(&guest.device_id)?;
    service.challenges.cancel_for_device(&guest.device_id);
    service.rooms.kick_shared_viewers(&guest.device_id).await;
    tracing::info!(guest_id = %guest.id, device_id = %guest.device_id, "guest share cancelled");
    Ok(api::Ok(Empty {}))
}

/// `POST /api/v1/guest/room/share/upload_control_mode`.
pub async fn guest_upload_control_mode(
    guest: Guest,
    State(service): State<ShareService>,
    Body(req): Body<UploadControlModeRequest>,
) -> Result<api::Ok<Empty>, ApiError> {
    service
        .challenges
        .complete_control_mode(&guest.device_id, req);
    Ok(api::Ok(Empty {}))
}

/// `POST /api/v1/guest/room/share/upload/sign`.
pub async fn guest_upload_sign(
    guest: Guest,
    State(service): State<ShareService>,
    Body(req): Body<UploadShareSignRequest>,
) -> Result<api::Ok<Empty>, ApiError> {
    service.challenges.complete_sign(&guest.device_id, req);
    Ok(api::Ok(Empty {}))
}

/// `POST /api/v1/guest/room/share/confirmation`.
pub async fn guest_confirmation(
    guest: Guest,
    State(service): State<ShareService>,
    Body(req): Body<ShareConfirmationRequest>,
) -> Result<api::Ok<Empty>, ApiError> {
    service
        .challenges
        .decide_confirmation(&guest.device_id, &req.control_id, req.allow);
    Ok(api::Ok(Empty {}))
}

/// `POST /api/v1/device/share/info/refresh_id` and legacy `POST /api/v1/device/share/info/refresh`.
pub async fn device_share_info_refresh(
    user: User,
    CallerDevice(caller): CallerDevice,
    State(service): State<ShareService>,
    OptionalBody(_body): OptionalBody<Value>,
) -> Result<api::Ok<DeviceShareInfoResponse>, ApiError> {
    let device_id = caller.ok_or(ApiError::UNKNOWN_DEVICE)?;
    let device = service
        .devices
        .get_for_caller(&device_id, &user.id)?
        .ok_or(ApiError::UNKNOWN_DEVICE)?;
    let connect_id = service.shares.refresh_share_id(&device.id, None)?;
    let alias = if device.name.is_empty() {
        device.id
    } else {
        device.name
    };
    Ok(api::Ok(DeviceShareInfoResponse {
        connect_id,
        connect_code: String::new(),
        alias,
        extra: Map::new(),
    }))
}

/// `POST /api/v1/device/share/cancel`.
pub async fn device_share_cancel(
    user: User,
    CallerDevice(caller): CallerDevice,
    State(service): State<ShareService>,
    OptionalBody(_body): OptionalBody<Value>,
) -> Result<api::Ok<Empty>, ApiError> {
    let device_id = caller.ok_or(ApiError::UNKNOWN_DEVICE)?;
    let _device = service
        .devices
        .get_for_caller(&device_id, &user.id)?
        .ok_or(ApiError::UNKNOWN_DEVICE)?;
    service.shares.cancel_share(&device_id)?;
    service.challenges.cancel_for_device(&device_id);
    service.rooms.kick_shared_viewers(&device_id).await;
    tracing::info!(user_id = %user.id, %device_id, "device share cancelled");
    Ok(api::Ok(Empty {}))
}

/// `POST /api/v1/device/share/{id}/unbind`: removes a shared device from the caller's list.
pub async fn device_share_unbind(
    user: User,
    Path(id): Path<String>,
    State(service): State<ShareService>,
    OptionalBody(_body): OptionalBody<Value>,
) -> Result<api::Ok<Empty>, ApiError> {
    if service.shares.unbind_shared(&user.id, &id)?.is_none() {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    tracing::info!(user_id = %user.id, target = %id, "shared device unbound");
    Ok(api::Ok(Empty {}))
}

#[derive(Deserialize, Default)]
pub struct SavedQuery {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn saved_response(
    user: User,
    service: ShareService,
    kind: SavedKind,
    query: SavedQuery,
) -> Result<api::Ok<AssistDeviceRecordPage>, ApiError> {
    let offset = query
        .cursor
        .as_deref()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let page = service.shares.list_saved(&user.id, kind, limit, offset)?;
    Ok(api::Ok(page))
}

/// `GET /api/v1/device/share/saved`.
pub async fn device_share_saved(
    user: User,
    Query(query): Query<SavedQuery>,
    State(service): State<ShareService>,
) -> Result<api::Ok<AssistDeviceRecordPage>, ApiError> {
    saved_response(user, service, SavedKind::All, query).await
}

/// `GET /api/v1/device/share/saved/favorites`.
pub async fn device_share_saved_favorites_get(
    user: User,
    Query(query): Query<SavedQuery>,
    State(service): State<ShareService>,
) -> Result<api::Ok<AssistDeviceRecordPage>, ApiError> {
    saved_response(user, service, SavedKind::Favorites, query).await
}

/// `GET /api/v1/device/share/saved/{kind}` (`/recent` or `/favorites`).
pub async fn device_share_saved_kind(
    user: User,
    Path(kind): Path<String>,
    Query(query): Query<SavedQuery>,
    State(service): State<ShareService>,
) -> Result<api::Ok<AssistDeviceRecordPage>, ApiError> {
    let kind = match kind.as_str() {
        "favorites" => SavedKind::Favorites,
        "recent" => SavedKind::Recent,
        _ => return Err(ApiError::INVALID_REQUEST),
    };
    saved_response(user, service, kind, query).await
}

/// `POST /api/v1/device/share/saved/favorites`.
pub async fn device_share_saved_favorites_post(
    user: User,
    State(service): State<ShareService>,
    Body(req): Body<AssistFavoritesRequest>,
) -> Result<api::Ok<Empty>, ApiError> {
    service.shares.save_favorites(&user.id, &req)?;
    Ok(api::Ok(Empty {}))
}

/// `DELETE /api/v1/device/share/saved/{kind}/{publisher_device_id}`.
pub async fn device_share_saved_delete(
    user: User,
    Path((kind, publisher_device_id)): Path<(String, String)>,
    State(service): State<ShareService>,
) -> Result<api::Ok<Empty>, ApiError> {
    let kind = match kind.as_str() {
        "favorites" => SavedKind::Favorites,
        "recent" => SavedKind::Recent,
        _ => return Err(ApiError::INVALID_REQUEST),
    };
    service
        .shares
        .delete_saved(&user.id, kind, &publisher_device_id)?;
    Ok(api::Ok(Empty {}))
}

/// `POST /api/v2/room/share/upload_control_mode` (account host).
pub async fn room_share_upload_control_mode(
    user: User,
    CallerDevice(caller): CallerDevice,
    State(service): State<ShareService>,
    Body(req): Body<UploadControlModeRequest>,
) -> Result<api::Ok<Empty>, ApiError> {
    let device_id = caller.ok_or(ApiError::UNKNOWN_DEVICE)?;
    if service
        .devices
        .get_for_caller(&device_id, &user.id)?
        .is_none()
    {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    service.challenges.complete_control_mode(&device_id, req);
    Ok(api::Ok(Empty {}))
}

/// `POST /api/v2/room/share/upload_sign` (account host).
pub async fn room_share_upload_sign(
    user: User,
    CallerDevice(caller): CallerDevice,
    State(service): State<ShareService>,
    Body(req): Body<UploadShareSignRequest>,
) -> Result<api::Ok<Empty>, ApiError> {
    let device_id = caller.ok_or(ApiError::UNKNOWN_DEVICE)?;
    if service
        .devices
        .get_for_caller(&device_id, &user.id)?
        .is_none()
    {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    service.challenges.complete_sign(&device_id, req);
    Ok(api::Ok(Empty {}))
}

/// `POST /api/v2/room/share/confirmation` (account host).
pub async fn room_share_confirmation(
    user: User,
    CallerDevice(caller): CallerDevice,
    State(service): State<ShareService>,
    Body(req): Body<ShareConfirmationRequest>,
) -> Result<api::Ok<Empty>, ApiError> {
    let device_id = caller.ok_or(ApiError::UNKNOWN_DEVICE)?;
    if service
        .devices
        .get_for_caller(&device_id, &user.id)?
        .is_none()
    {
        return Err(ApiError::UNKNOWN_DEVICE);
    }
    service
        .challenges
        .decide_confirmation(&device_id, &req.control_id, req.allow);
    Ok(api::Ok(Empty {}))
}

async fn resolve_share_target(
    service: &ShareService,
    user: &User,
    connect_id: &str,
) -> Result<ShareTarget, ApiError> {
    let Some(target) = service.shares.resolve_by_connect_id(connect_id)? else {
        return Err(ApiError::INCORRECT_SHARE_CODE);
    };
    if target.owner.as_deref() == Some(user.id.as_str()) {
        return Err(ApiError::SAME_ACCOUNT_ASSIST);
    }
    if !target.active {
        return Err(ApiError::ASSIST_DISABLED);
    }
    if !service.rooms.is_online(&target.device_id).await {
        return Err(ApiError::DEVICE_OFFLINE);
    }
    Ok(target)
}

fn build_assist_room_config(
    service: &ShareService,
    user: &User,
    target: &ShareTarget,
    connect_id: &str,
    from_device: Option<String>,
    force_join: bool,
) -> AssistRoomConfig {
    let (token, signaling_server) = service.tickets.issue(Room::shared_viewer(
        user.id.clone(),
        target.device_id.clone(),
        from_device,
        force_join,
    ));
    let sni_required_signaling_list = if service.tickets.public_url.requires_sni() {
        vec![signaling_server.clone()]
    } else {
        Vec::new()
    };
    AssistRoomConfig {
        signaling_list: vec![signaling_server.clone()],
        signaling_server,
        sni_required_signaling_list,
        token,
        ws_connect_timeout_ms: WS_CONNECT_TIMEOUT_MS,
        streamer_retry_delta_ms: STREAMER_RETRY_DELTA_MS,
        max_reconnect_delta: MAX_RECONNECT_DELTA_SECS,
        report_token: String::new(),
        report_url: String::new(),
        report_server_address: Some(String::new()),
        share_id: connect_id.to_owned(),
        device_name: target.name.clone(),
        publisher_version_name: target.version_name.clone(),
        publisher_platform: target.platform,
        publisher_device_id: target.device_id.clone(),
        international_connect: false,
        device_binding: if target.owner.is_some() {
            "user_device".to_owned()
        } else {
            String::new()
        },
        streamer_config: "{}".to_owned(),
        extra: Map::new(),
    }
}

/// `POST /api/v2/room/share/control_mode`: queries the target host's assist mode via
/// `bmsg_push` `get_control_mode`.
pub async fn room_share_control_mode(
    user: User,
    State(service): State<ShareService>,
    Body(req): Body<ShareControlModeRequest>,
) -> Result<api::Ok<ShareControlModeResponse>, ApiError> {
    let target = resolve_share_target(&service, &user, &req.connect_id).await?;
    let (control_id, rx) = service.challenges.register_control_mode(&target.device_id);
    let push = json!({
        "type": "get_control_mode",
        "data": { "control_id": control_id },
    });
    if !service
        .rooms
        .push_to_host(&target.device_id, "bmsg_push", push)
        .await
    {
        service.challenges.remove_control_mode(&control_id);
        return Err(ApiError::DEVICE_OFFLINE);
    }

    let timeout = if target.platform == 4 {
        MACOS_CONTROL_MODE_GRACE
    } else {
        CHALLENGE_TIMEOUT
    };
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(reply)) => {
            let need_password = reply.allow_control && reply.control_mode.needs_password();
            Ok(api::Ok(ShareControlModeResponse {
                can_remote_control: Some(reply.allow_control),
                allow_control: Some(reply.allow_control),
                control_mode: reply.control_mode,
                need_password,
                control_id: Some(reply.control_id),
                extra: Map::new(),
            }))
        }
        _ => {
            service.challenges.remove_control_mode(&control_id);
            if target.platform == 4 {
                Ok(api::Ok(ShareControlModeResponse {
                    can_remote_control: Some(target.controllable),
                    allow_control: Some(target.controllable),
                    control_mode: ShareControlMode::ByPassword,
                    need_password: target.controllable,
                    control_id: Some(control_id),
                    extra: Map::new(),
                }))
            } else {
                Err(ApiError::ASSIST_DISABLED)
            }
        }
    }
}

/// `POST /api/v1/room/join/share/by_code` and `POST /api/v2/room/join/share/by_code`.
pub async fn room_join_share_by_code(
    user: User,
    ClientAddress(address): ClientAddress,
    CallerDevice(from_device): CallerDevice,
    State(service): State<ShareService>,
    Body(req): Body<JoinShareByCodeRequest>,
) -> Result<api::Ok<AssistRoomConfig>, ApiError> {
    if !service.limits.share_join(address) {
        return Err(ApiError::TOO_MANY_REQUESTS);
    }
    if req.connect_id.trim().is_empty() || req.connect_code.trim().is_empty() {
        return Err(ApiError::INCORRECT_SHARE_CODE);
    }
    let target = resolve_share_target(&service, &user, &req.connect_id).await?;
    if !req.force_join
        && service
            .rooms
            .controlled_elsewhere(&target.device_id, from_device.as_deref())
            .await
    {
        return Err(ApiError::BEING_CONTROLLED);
    }

    let (control_id, salt, rx) = service.challenges.register_sign(&target.device_id);
    let push = json!({
        "type": "remote_control",
        "data": { "control_id": control_id, "salt": salt },
    });
    if !service
        .rooms
        .push_to_host(&target.device_id, "bmsg_push", push)
        .await
    {
        service.challenges.remove_sign(&control_id);
        return Err(ApiError::DEVICE_OFFLINE);
    }

    let reply = match tokio::time::timeout(CHALLENGE_TIMEOUT, rx).await {
        Ok(Ok(reply)) => reply,
        _ => {
            service.challenges.remove_sign(&control_id);
            return Err(ApiError::ASSIST_DISABLED);
        }
    };
    if !reply.can_remote_control {
        return Err(ApiError::ASSIST_DISABLED);
    }
    let primary_ok = verify_share_code_digest(&salt, &req.connect_code, &reply.sign);
    let backup_ok = !reply.backup_sign.is_empty()
        && verify_share_code_digest(&salt, &req.connect_code, &reply.backup_sign);
    if !primary_ok && !backup_ok {
        return Err(ApiError::INCORRECT_SHARE_CODE);
    }

    service
        .shares
        .record_shared_connection(&user.id, &target.device_id)?;
    tracing::info!(
        user_id = %user.id,
        device_id = %target.device_id,
        connect_id = %req.connect_id,
        "assist room joined by code"
    );
    Ok(api::Ok(build_assist_room_config(
        &service,
        &user,
        &target,
        &req.connect_id,
        from_device,
        req.force_join,
    )))
}

/// `POST /api/v2/room/join/share/by_confirmation`.
pub async fn room_join_share_by_confirmation(
    user: User,
    ClientAddress(address): ClientAddress,
    CallerDevice(from_device): CallerDevice,
    State(service): State<ShareService>,
    Body(req): Body<JoinShareByConfirmationRequest>,
) -> Result<Response, ApiError> {
    if !service.limits.share_join(address) {
        return Err(ApiError::TOO_MANY_REQUESTS);
    }
    let target = resolve_share_target(&service, &user, &req.connect_id).await?;
    if !req.force_join
        && service
            .rooms
            .controlled_elsewhere(&target.device_id, from_device.as_deref())
            .await
    {
        return Err(ApiError::BEING_CONTROLLED);
    }

    let caller_name = match from_device.as_deref() {
        Some(id) => service
            .devices
            .get(id)?
            .map(|d| d.name)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| id.to_owned()),
        None => format!("User {}", user.id),
    };

    let (control_id, status, newly_created) = service.challenges.poll_or_start_confirmation(
        &user.id,
        &req.connect_id,
        &target.device_id,
        req.control_id.as_deref(),
    );
    if newly_created {
        let push = json!({
            "type": "remote_control_by_confirmation",
            "data": {
                "control_id": control_id,
                "device_name": caller_name,
                "user_id": user.id,
            },
        });
        if !service
            .rooms
            .push_to_host(&target.device_id, "bmsg_push", push)
            .await
        {
            service
                .challenges
                .cancel_for_viewer(&user.id, &req.connect_id, Some(&control_id));
            return Err(ApiError::DEVICE_OFFLINE);
        }
    }

    match status {
        ConfirmationStatus::Pending => Ok(ApiError::SHARE_CONFIRMATION_REQUIRED
            .into_response_with_data(json!({ "control_id": control_id }))),
        ConfirmationStatus::Denied => Err(ApiError::SHARE_CONFIRMATION_DENIED),
        ConfirmationStatus::Approved => {
            service
                .shares
                .record_shared_connection(&user.id, &target.device_id)?;
            tracing::info!(
                user_id = %user.id,
                device_id = %target.device_id,
                connect_id = %req.connect_id,
                "assist room joined by confirmation"
            );
            Ok(api::Ok(build_assist_room_config(
                &service,
                &user,
                &target,
                &req.connect_id,
                from_device,
                req.force_join,
            ))
            .into_response())
        }
    }
}

/// `POST /api/v2/room/share/cancel_remote_assist`.
pub async fn room_share_cancel_remote_assist(
    user: User,
    State(service): State<ShareService>,
    Body(req): Body<CancelRemoteAssistRequest>,
) -> Result<api::Ok<Empty>, ApiError> {
    let cancelled =
        service
            .challenges
            .cancel_for_viewer(&user.id, &req.connect_id, req.control_id.as_deref());
    for (device_id, control_id) in cancelled {
        let push = json!({
            "type": "remote_control_by_confirmation_cancel",
            "data": { "control_id": control_id },
        });
        let _ = service
            .rooms
            .push_to_host(&device_id, "bmsg_push", push)
            .await;
    }
    Ok(api::Ok(Empty {}))
}
