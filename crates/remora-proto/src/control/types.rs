//! Cloud REST API response envelope, errors, and control-plane message types.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Result of one cloud call, from the HTTP status and the `{code, msg, data}` body.
///
/// Success requires HTTP 200 *and* `code == 0`. `msg` is never kept: for auth calls it can
/// contain tickets.
pub fn parse_response<T: DeserializeOwned>(http_status: u16, body: &[u8]) -> Result<T, ApiError> {
    let envelope: Envelope =
        serde_json::from_slice(body).map_err(|_| ApiError::Malformed { http_status })?;
    match (http_status, envelope.code) {
        (200, 0) => T::deserialize(envelope.data).map_err(|_| ApiError::Malformed { http_status }),
        (_, 1120) => Err(ApiError::TokenExpired),
        (_, 1190) => Err(ApiError::QrCodeExpired),
        (_, code) => Err(ApiError::Rejected { http_status, code }),
    }
}

#[derive(Deserialize)]
struct Envelope {
    code: i64,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApiError {
    /// The user token is invalid or expired: log in again, do not retry.
    TokenExpired,
    /// The login QR code expired: generate a new one.
    QrCodeExpired,
    /// Any other non-zero code, e.g. 1001 for an invalid request body.
    Rejected { http_status: u16, code: i64 },
    /// The body is not an envelope, or `data` has an unexpected shape.
    Malformed { http_status: u16 },
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TokenExpired => f.write_str("login expired"),
            Self::QrCodeExpired => f.write_str("QR code expired"),
            Self::Rejected { http_status, code } => {
                write!(
                    f,
                    "cloud rejected the request: HTTP {http_status}, code {code}"
                )
            }
            Self::Malformed { http_status } => {
                write!(f, "malformed cloud response: HTTP {http_status}")
            }
        }
    }
}

impl std::error::Error for ApiError {}

/// Request body for `POST /api/v1/device/{platform}/init`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInitRequest {
    pub client_id: String,
    #[serde(default)]
    pub name: String,
}

/// Response `data` from `POST /api/v1/device/{platform}/init`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInitResponse {
    pub device_id: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Response `data` from `POST /api/v1/qrcode/gen/login`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct QrLoginGenResponse {
    pub qrcode_jump_url: String,
    pub status_query_ticket: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl fmt::Debug for QrLoginGenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QrLoginGenResponse")
            .field("qrcode_jump_url", &"<redacted>")
            .field("status_query_ticket", &"<redacted>")
            .finish()
    }
}

/// Semantic QR login status codes returned by `/api/v1/qrcode/login/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum QrCodeStatus {
    Waiting,
    Scanned,
    Expired,
    Confirmed,
    Unknown(u8),
}

impl From<u8> for QrCodeStatus {
    fn from(value: u8) -> Self {
        match value {
            1 => Self::Waiting,
            2 => Self::Scanned,
            3 => Self::Expired,
            4 => Self::Confirmed,
            other => Self::Unknown(other),
        }
    }
}

impl From<QrCodeStatus> for u8 {
    fn from(status: QrCodeStatus) -> Self {
        match status {
            QrCodeStatus::Waiting => 1,
            QrCodeStatus::Scanned => 2,
            QrCodeStatus::Expired => 3,
            QrCodeStatus::Confirmed => 4,
            QrCodeStatus::Unknown(v) => v,
        }
    }
}

/// Request body for `POST /api/v1/qrcode/login/status`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QrLoginStatusRequest {
    pub token: String,
    pub qrcode_jump_url: String,
    #[serde(default)]
    pub login_status: u8,
}

impl fmt::Debug for QrLoginStatusRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QrLoginStatusRequest")
            .field("token", &"<redacted>")
            .field("qrcode_jump_url", &"<redacted>")
            .field("login_status", &self.login_status)
            .finish()
    }
}

/// Response `data` from `POST /api/v1/qrcode/login/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QrLoginStatusResponse {
    pub status: u8,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Request body for `POST /api/v1/login/by_qrcode`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginByQrcodeRequest {
    pub token: String,
    pub qrcode_jump_url: String,
}

impl fmt::Debug for LoginByQrcodeRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginByQrcodeRequest")
            .field("token", &"<redacted>")
            .field("qrcode_jump_url", &"<redacted>")
            .finish()
    }
}

/// Response `data` from `POST /api/v1/login/by_qrcode`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub user_id: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl fmt::Debug for LoginResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginResponse")
            .field("token", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("nickname", &self.nickname)
            .finish()
    }
}

/// Response `data` from `GET /api/v1/user/info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserInfoResponse {
    pub user_id: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Online status of a device in `/api/v1/device/list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum DeviceStatus {
    Connected,
    Disconnected,
    #[serde(other)]
    Unknown,
}

/// An active viewer session entry inside [`DeviceEntry::participants_info`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParticipantInfo {
    pub client_id: String,
    pub device_id: String,
    #[serde(default)]
    pub alias: Option<String>,
    pub platform: u8,
    #[serde(default)]
    pub user_join_type: u8,
    #[serde(default)]
    pub controlled_time: u64,
    #[serde(default)]
    pub app_flag: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A single device entry in `/api/v1/device/list` and `/api/v1/device/groups/of/my`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub device_id: String,
    #[serde(default)]
    pub alias: Option<String>,
    pub platform: u8,
    pub status: DeviceStatus,
    #[serde(default)]
    pub participants_info: Vec<ParticipantInfo>,
    #[serde(default)]
    pub wallpaper_url: Option<String>,
    #[serde(default)]
    pub version_name: String,
    #[serde(default)]
    pub support_wol: bool,
    #[serde(default)]
    pub controllable: Option<bool>,
    #[serde(default)]
    pub publisher_availability_status: Option<String>,
    #[serde(default)]
    pub controlled_support: bool,
    #[serde(default)]
    pub update_started_at: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Response `data` from `GET /api/v1/device/list`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceListResponse {
    #[serde(default)]
    pub current_device: Option<DeviceEntry>,
    #[serde(default)]
    pub my_binded_devices: Vec<DeviceEntry>,
    #[serde(default)]
    pub others_shared_devices: Vec<DeviceEntry>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Response `data` from `GET /api/v1/device/groups/of/my`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceGroupsResponse {
    #[serde(default)]
    pub current_device_id: Option<String>,
    #[serde(default)]
    pub mobile_devices: Vec<DeviceEntry>,
    #[serde(default)]
    pub desktop_devices: Vec<DeviceEntry>,
    #[serde(default)]
    pub tv_devices: Vec<DeviceEntry>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Request body for `POST /api/v1/device/mac_controllable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacControllableRequest {
    pub controllable: bool,
}

/// Tool bar order inside [`DevicePreferences`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceTools {
    #[serde(default)]
    pub sequence: Vec<String>,
}

/// Per-device preferences object inside [`DevicePreferencesResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePreferences {
    pub device_id: String,
    #[serde(default)]
    pub tools: DeviceTools,
}

/// Response `data` from `GET /api/v1/device/{device_id}/preferences`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePreferencesResponse {
    pub device_preferences: DevicePreferences,
}

/// Response `data` from `GET /api/v1/device/share/info`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceShareInfoResponse {
    pub connect_id: String,
    #[serde(default)]
    pub connect_code: String,
    #[serde(default)]
    pub alias: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl fmt::Debug for DeviceShareInfoResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceShareInfoResponse")
            .field("connect_id", &self.connect_id)
            .field("connect_code", &"<redacted>")
            .field("alias", &self.alias)
            .finish()
    }
}

/// Response `data` from `GET /api/v1/release/fetch/update` and
/// `GET /api/v1/release/fetch/update/{device_id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseUpdateResponse {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub version_name: String,
    #[serde(default)]
    pub version_code: u64,
    #[serde(default)]
    pub change_logs: Vec<String>,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub user_external_force_upgrade: bool,
    #[serde(default)]
    pub upgrade_uri: Map<String, Value>,
    #[serde(default)]
    pub update_type: String,
    #[serde(default)]
    pub update_prompt_type: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Optional request body for `POST /api/v1/room/create`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomCreateRequest {
    #[serde(default)]
    pub last_controlled_interval: Option<i64>,
}

/// Response `data` from `POST /api/v1/room/create`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomCreateResponse {
    pub signaling_server: String,
    #[serde(default)]
    pub signaling_list: Vec<String>,
    pub token: String,
    pub ws_connect_timeout_ms: u32,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl fmt::Debug for RoomCreateResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomCreateResponse")
            .field("signaling_server", &self.signaling_server)
            .field("signaling_list", &self.signaling_list)
            .field("token", &"<redacted>")
            .field("ws_connect_timeout_ms", &self.ws_connect_timeout_ms)
            .finish()
    }
}

/// Request body for `POST /api/v1/room/join/by_device/{device_id}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomJoinRequest {
    #[serde(default)]
    pub force_join: bool,
}

/// Response `data` from `POST /api/v1/room/join/by_device/{device_id}`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomJoinResponse {
    #[serde(default)]
    pub signaling_list: Vec<String>,
    pub token: String,
    pub ws_connect_timeout_ms: u32,
    #[serde(default)]
    pub report_token: String,
    #[serde(default)]
    pub report_url: String,
    pub publisher_platform: u8,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl fmt::Debug for RoomJoinResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomJoinResponse")
            .field("signaling_list", &self.signaling_list)
            .field("token", &"<redacted>")
            .field("ws_connect_timeout_ms", &self.ws_connect_timeout_ms)
            .field("report_token", &"<redacted>")
            .field("report_url", &self.report_url)
            .field("publisher_platform", &self.publisher_platform)
            .finish()
    }
}

/// Response `data` from `POST /api/v1/guest/create`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct GuestCreateResponse {
    pub guest_id: String,
    pub token: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl fmt::Debug for GuestCreateResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuestCreateResponse")
            .field("guest_id", &self.guest_id)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Verification mode for remote assistance (`by_password`, `by_confirmation`, `password_confirmation`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShareControlMode {
    ByPassword,
    ByConfirmation,
    PasswordConfirmation,
    #[serde(untagged)]
    Other(String),
}

impl ShareControlMode {
    pub fn as_str(&self) -> &str {
        match self {
            Self::ByPassword => "by_password",
            Self::ByConfirmation => "by_confirmation",
            Self::PasswordConfirmation => "password_confirmation",
            Self::Other(s) => s.as_str(),
        }
    }

    pub fn needs_password(&self) -> bool {
        matches!(self, Self::ByPassword | Self::PasswordConfirmation)
    }

    pub fn allows_confirmation(&self) -> bool {
        matches!(self, Self::ByConfirmation | Self::PasswordConfirmation)
    }
}

/// Request body for `POST /api/v2/room/share/control_mode`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareControlModeRequest {
    pub connect_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_id: Option<String>,
}

/// Response `data` from `POST /api/v2/room/share/control_mode`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareControlModeResponse {
    pub allow_control: bool,
    pub control_mode: ShareControlMode,
    pub need_password: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Request body for `POST /api/v1/guest/room/share/upload_control_mode` and
/// `POST /api/v2/room/share/upload_control_mode`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadControlModeRequest {
    pub control_id: String,
    pub allow_control: bool,
    pub control_mode: ShareControlMode,
}

/// Request body for `POST /api/v1/guest/room/share/upload/sign` and
/// `POST /api/v2/room/share/upload_sign`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadShareSignRequest {
    pub control_id: String,
    pub can_remote_control: bool,
    #[serde(default)]
    pub sign: String,
    #[serde(default)]
    pub backup_sign: String,
}

impl fmt::Debug for UploadShareSignRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadShareSignRequest")
            .field("control_id", &self.control_id)
            .field("can_remote_control", &self.can_remote_control)
            .field("sign", &"<redacted>")
            .field("has_backup_sign", &(!self.backup_sign.is_empty()))
            .finish()
    }
}

/// Request body for `POST /api/v1/guest/room/share/confirmation` and
/// `POST /api/v2/room/share/confirmation`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareConfirmationRequest {
    pub control_id: String,
    #[serde(default, alias = "can_remote_control", alias = "confirm")]
    pub allow: bool,
}

/// Request body for `POST /api/v1/room/join/share/by_code` and
/// `POST /api/v2/room/join/share/by_code`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinShareByCodeRequest {
    pub connect_id: String,
    pub connect_code: String,
    #[serde(default)]
    pub force_join: bool,
}

impl fmt::Debug for JoinShareByCodeRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinShareByCodeRequest")
            .field("connect_id", &self.connect_id)
            .field("connect_code", &"<redacted>")
            .field("force_join", &self.force_join)
            .finish()
    }
}

/// Request body for `POST /api/v2/room/join/share/by_confirmation`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinShareByConfirmationRequest {
    pub connect_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_id: Option<String>,
    #[serde(default)]
    pub force_join: bool,
}

/// Request body for `POST /api/v2/room/share/cancel_remote_assist`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRemoteAssistRequest {
    pub connect_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_id: Option<String>,
}

/// Response `data` from `POST /api/v{1,2}/room/join/share/by_code` and
/// `POST /api/v2/room/join/share/by_confirmation`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistRoomConfig {
    pub signaling_server: String,
    #[serde(default)]
    pub signaling_list: Vec<String>,
    #[serde(default)]
    pub sni_required_signaling_list: Vec<String>,
    pub token: String,
    pub ws_connect_timeout_ms: u32,
    pub streamer_retry_delta_ms: u32,
    pub max_reconnect_delta: u32,
    #[serde(default)]
    pub report_token: String,
    #[serde(default)]
    pub report_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_server_address: Option<String>,
    #[serde(default)]
    pub share_id: String,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub publisher_version_name: String,
    pub publisher_platform: u8,
    #[serde(default)]
    pub publisher_device_id: String,
    #[serde(default)]
    pub international_connect: bool,
    #[serde(default)]
    pub device_binding: String,
    #[serde(default)]
    pub streamer_config: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl fmt::Debug for AssistRoomConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistRoomConfig")
            .field("signaling_server", &self.signaling_server)
            .field("signaling_list", &self.signaling_list)
            .field("token", &"<redacted>")
            .field("share_id", &self.share_id)
            .field("publisher_platform", &self.publisher_platform)
            .field("publisher_device_id", &self.publisher_device_id)
            .finish()
    }
}

/// One saved/recent/favorite assisted device entry returned by `/api/v1/device/share/saved*`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistDeviceRecord {
    pub publisher_device_id: String,
    pub connect_id: String,
    #[serde(default)]
    pub platform: u8,
    #[serde(default)]
    pub remark: String,
    #[serde(default)]
    pub is_favorite: bool,
    #[serde(default)]
    pub last_connected_at: u64,
    #[serde(default)]
    pub favorited_at: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Paginated response `data` from `GET /api/v1/device/share/saved{,/favorites,/recent}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistDeviceRecordPage {
    #[serde(default)]
    pub saved: Vec<AssistDeviceRecord>,
    #[serde(default)]
    pub next: Option<String>,
}

/// One favorite item in `POST /api/v1/device/share/saved/favorites`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistFavoriteInput {
    pub connect_id: String,
    #[serde(default)]
    pub remark: String,
    #[serde(default)]
    pub favorited_at: u64,
}

/// Request body for `POST /api/v1/device/share/saved/favorites`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistFavoritesRequest {
    #[serde(default)]
    pub favorites: Vec<AssistFavoriteInput>,
    #[serde(default)]
    pub skip_invalid: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Deserialize)]
    struct Room {
        token: String,
    }

    #[test]
    fn parses_success_data() {
        let body = br#"{"code":0,"msg":"ok","data":{"token":"t","extra":1}}"#;
        assert_eq!(parse_response(200, body), Ok(Room { token: "t".into() }));
        assert_eq!(parse_response(200, br#"{"code":0,"msg":"ok"}"#), Ok(()));
    }

    #[test]
    fn classifies_errors() {
        let token = r#"{"code":1120,"msg":"Token无效或已过期"}"#.as_bytes();
        assert_eq!(
            parse_response::<()>(422, token),
            Err(ApiError::TokenExpired)
        );
        let qr = br#"{"code":1190,"msg":"expired"}"#;
        assert_eq!(parse_response::<()>(400, qr), Err(ApiError::QrCodeExpired));
        let invalid = br#"{"code":1001,"msg":"","data":[{"loc":["body"]}]}"#;
        assert_eq!(
            parse_response::<()>(422, invalid),
            Err(ApiError::Rejected {
                http_status: 422,
                code: 1001
            })
        );
        assert_eq!(
            parse_response::<()>(500, br#"{"code":0}"#),
            Err(ApiError::Rejected {
                http_status: 500,
                code: 0
            })
        );
    }

    #[test]
    fn rejects_malformed_bodies() {
        assert_eq!(
            parse_response::<()>(502, b"<html>"),
            Err(ApiError::Malformed { http_status: 502 })
        );
        assert_eq!(
            parse_response::<Room>(200, br#"{"code":0,"data":{}}"#),
            Err(ApiError::Malformed { http_status: 200 })
        );
    }

    #[test]
    fn debug_redacts_assist_secrets() {
        let guest = GuestCreateResponse {
            guest_id: "g-1".into(),
            token: "secret-guest-jwt".into(),
            extra: Map::new(),
        };
        assert!(!format!("{guest:?}").contains("secret-guest-jwt"));

        let join = JoinShareByCodeRequest {
            connect_id: "123456789".into(),
            connect_code: "SECRET88".into(),
            force_join: false,
        };
        assert!(!format!("{join:?}").contains("SECRET88"));

        let sign = UploadShareSignRequest {
            control_id: "c-1".into(),
            can_remote_control: true,
            sign: "deadbeef".into(),
            backup_sign: String::new(),
        };
        assert!(!format!("{sign:?}").contains("deadbeef"));

        let room = AssistRoomConfig {
            signaling_server: "wss://example.test/socket.io/".into(),
            signaling_list: vec!["wss://example.test/socket.io/".into()],
            sni_required_signaling_list: vec![],
            token: "secret-room-jwt".into(),
            ws_connect_timeout_ms: 16_000,
            streamer_retry_delta_ms: 1_000,
            max_reconnect_delta: 300,
            report_token: "secret-report".into(),
            report_url: String::new(),
            report_server_address: None,
            share_id: "123456789".into(),
            device_name: "host".into(),
            publisher_version_name: "4.48.2".into(),
            publisher_platform: 1,
            publisher_device_id: "dev-1".into(),
            international_connect: false,
            device_binding: String::new(),
            streamer_config: String::new(),
            extra: Map::new(),
        };
        let dbg = format!("{room:?}");
        assert!(!dbg.contains("secret-room-jwt"));
        assert!(!dbg.contains("secret-report"));
    }
}
