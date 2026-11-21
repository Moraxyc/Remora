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
}
