//! Shared profile data model used at both build time and runtime.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Operating system family of a UU Remote client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
    Ios,
    Android,
    #[serde(rename = "android_tv")]
    AndroidTv,
}

/// How the profile data was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProfileSource {
    /// Inferred or extrapolated from adjacent releases; evidence is optional.
    Inferred,
    /// Extracted by statically inspecting an official package or binary; requires evidence.
    ReverseEngineered,
    /// Observed from live wire captures against an official client; requires evidence.
    Captured,
}

/// Header-matching rule used to identify a client profile on incoming requests.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectRule {
    pub plat: String,
    pub version: String,
    #[serde(default)]
    pub build: Option<u32>,
}

/// Supported `FeatureFlag` revision levels advertised by a client build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureCaps {
    pub capture_setting: i32,
    pub simple_action: i32,
    pub system_metrics: i32,
    pub private_screen: i32,
    pub update_acquire: i32,
    pub file_transfer_ftp: i32,
    pub file_transfer_ftp2: i32,
    pub clipboard: i32,
    pub qos_stat: i32,
    pub mumu_control: i32,
    pub virtual_mouse_device: i32,
}

/// Wire and media capabilities of an official client build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caps {
    pub codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
    pub transports: Vec<String>,
    pub data_channels: Vec<String>,
    pub features: FeatureCaps,
}

/// Version-specific transport and protocol quirks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Quirks {
    pub kcp_quic_optional: bool,
    pub log_archive_push_only: bool,
    pub wallpaper_raw_post: bool,
    /// Uses `/api/v1/room/join/share/by_code` instead of `/api/v2/room/join/share/by_code`.
    pub join_share_v1: bool,
    /// Host reports assist control mode via `.../upload_control_mode`.
    pub guest_upload_control_mode: bool,
    /// Only queries `/api/v1/device/share/saved/favorites` rather than `/saved` or `/recent`.
    pub share_saved_favorites_only: bool,
    /// Retains the legacy `/api/v1/device/share/info/refresh` route alongside `refresh_id`.
    pub share_refresh_legacy: bool,
    /// Queries `/api/v1/member/entitlements` at startup.
    pub entitlements: bool,
    /// Queries `/api/v1/device/{id}/preferences`.
    pub device_preferences: bool,
    /// Validates `SHA1withRSA` `X-Param-Sign` response headers on `/api/v1/user/info`.
    pub response_sign_user_info: bool,
}

/// Engine.IO / Socket.IO signaling timing and reconnect backoff parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SignalProfile {
    pub path: String,
    pub handshake_timeout_ms: u64,
    pub ping_interval_ms: u64,
    pub ping_timeout_ms: u64,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,
    pub reconnect_key_refresh_ms: u64,
}

impl Default for SignalProfile {
    fn default() -> Self {
        Self {
            path: "/socket.io/?EIO=4&transport=websocket".to_owned(),
            handshake_timeout_ms: 20_000,
            ping_interval_ms: 25_000,
            ping_timeout_ms: 20_000,
            reconnect_initial_ms: 1_000,
            reconnect_max_ms: 30_000,
            reconnect_key_refresh_ms: 300_000,
        }
    }
}

impl SignalProfile {
    pub const fn handshake_timeout(&self) -> Duration {
        Duration::from_millis(self.handshake_timeout_ms)
    }

    pub const fn ping_interval(&self) -> Duration {
        Duration::from_millis(self.ping_interval_ms)
    }

    pub const fn ping_timeout(&self) -> Duration {
        Duration::from_millis(self.ping_timeout_ms)
    }

    pub fn liveness_timeout(&self) -> Duration {
        let ms = self.ping_interval_ms.saturating_add(self.ping_timeout_ms);
        Duration::from_millis(ms).clamp(Duration::from_secs(5), Duration::from_secs(300))
    }

    pub const fn reconnect_initial(&self) -> Duration {
        Duration::from_millis(self.reconnect_initial_ms)
    }

    pub const fn reconnect_max(&self) -> Duration {
        Duration::from_millis(self.reconnect_max_ms)
    }

    pub const fn reconnect_key_refresh(&self) -> Duration {
        Duration::from_millis(self.reconnect_key_refresh_ms)
    }
}

/// Static description of an official client release, embedded from `profiles/*.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    #[serde(default)]
    pub inherits: Option<String>,
    pub source: ProfileSource,
    pub evidence: Vec<String>,
    pub platform: Platform,
    /// `X-Param-PLAT` wire value (e.g. `"4"` for macOS).
    pub plat: String,
    /// `X-Param-VN` wire value (e.g. `"4.48.2"`).
    pub version: String,
    /// `X-Param-VC` build number (e.g. `634`).
    pub build: u32,
    pub bundle_id: String,
    pub channel: String,
    pub min_os: String,
    pub init_endpoint: String,
    pub sign_headers: Vec<String>,
    pub detect: DetectRule,
    pub caps: Caps,
    pub quirks: Quirks,
    #[serde(default)]
    pub signal: SignalProfile,
}
