//! Client compatibility endpoints queried by official desktop apps on startup and session entry.

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::auth::token::now;
use crate::api::user::User;
use crate::api::{self, ApiError, Body};

fn caller_platform(headers: &HeaderMap) -> u8 {
    headers
        .get("x-param-plat")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(1)
}

fn feature_period(checkpoint_at: u64, limit: Value) -> Value {
    json!({
        "periods": [
            {
                "start_time": null,
                "end_time": checkpoint_at,
                "grant_type": 0,
                "limit": limit,
            }
        ]
    })
}

/// `GET /api/v1/member/entitlements`: grants full self-hosted streaming and input capabilities
/// so official viewers do not clamp quality, FPS, bitrate, or secondary screens.
pub async fn member_entitlements(
    user: User,
    headers: HeaderMap,
) -> Result<api::Ok<Value>, ApiError> {
    let ts = now();
    let expired_at = ts.saturating_add(1800);
    let checkpoint_at = ts.saturating_add(365 * 24 * 3600);
    let platform = caller_platform(&headers);

    Ok(api::Ok(json!({
        "version": 0,
        "expired_at": expired_at,
        "user_id": user.id,
        "platform": platform,
        "features": {
            "keymapping": feature_period(checkpoint_at, json!({ "enable": true })),
            "device_gamepad": feature_period(checkpoint_at, json!({ "enable": true })),
            "device_keyboard": feature_period(checkpoint_at, json!({ "enable": true })),
            "device_mouse": feature_period(checkpoint_at, json!({ "enable": true })),
            "second_screen": feature_period(checkpoint_at, json!({ "enable": true })),
            "screen_quality": feature_period(checkpoint_at, json!({ "quality": ["general", "hd", "bluray"] })),
            "screen_refresh_rate": feature_period(checkpoint_at, json!({ "fps": [30, 60, 90, 144] })),
            "screen_true_color": feature_period(checkpoint_at, json!({ "enable": true })),
            "screen_hdr": feature_period(checkpoint_at, json!({ "enable": true })),
            "screen_custom_bitrate": feature_period(checkpoint_at, json!({ "max_bitrate": 500_000_000 })),
            "relay_connection": feature_period(checkpoint_at, json!({ "enable": true })),
            "file_transport": feature_period(checkpoint_at, json!({ "max_total_speed": null })),
            "international_connect": feature_period(checkpoint_at, json!({ "enable": true })),
            "multi_window": feature_period(checkpoint_at, json!({ "enable": true })),
            "screen_virtual": feature_period(checkpoint_at, json!({ "enable": true })),
            "screen_super": feature_period(checkpoint_at, json!({ "enable": true })),
            "screensaver": feature_period(checkpoint_at, json!({ "preset_ids": null, "allow_custom": true })),
            "privacy_enhanced": feature_period(checkpoint_at, json!({ "enable": true })),
            "viewing_mode": feature_period(checkpoint_at, json!({ "enable": true })),
            "screen_wall": feature_period(checkpoint_at, json!({ "enable": true })),
            "clipboard_file_transport": feature_period(checkpoint_at, json!({ "max_total_speed": null })),
            "multi_channel": feature_period(checkpoint_at, json!({ "enable": true })),
        },
        "checkpoint_at": checkpoint_at,
    })))
}

#[derive(Deserialize)]
pub struct ConfigureQuery {
    name: String,
    #[serde(default)]
    version: String,
}

#[derive(Deserialize)]
pub struct ConfiguresRequest {
    #[serde(default)]
    configures: Vec<ConfigureQuery>,
}

#[derive(Serialize)]
pub struct ConfigureEntry {
    version: String,
    status: u8,
    data: Option<Value>,
}

/// `POST /api/v1/tool/configures_with_version`: tells clients to keep their built-in defaults.
pub async fn configures_with_version(
    Body(request): Body<ConfiguresRequest>,
) -> Result<api::Ok<BTreeMap<String, ConfigureEntry>>, ApiError> {
    let mut out = BTreeMap::new();
    for item in request.configures {
        if item.name.is_empty() {
            continue;
        }
        let status = if item.version.is_empty() { 2 } else { 1 };
        out.insert(
            item.name,
            ConfigureEntry {
                version: item.version,
                status,
                data: None,
            },
        );
    }
    Ok(api::Ok(out))
}

#[derive(Serialize)]
pub struct UiSwitchResponse {
    switch: bool,
}

/// `GET /api/v1/cloud_pc/ui_switch`.
pub async fn cloud_pc_ui_switch(_user: User) -> Result<api::Ok<UiSwitchResponse>, ApiError> {
    Ok(api::Ok(UiSwitchResponse { switch: false }))
}

#[derive(Serialize)]
pub struct MessageSummaryResponse {
    no_read_num: u64,
}

/// `GET /api/v1/message/summary`.
pub async fn message_summary(_user: User) -> Result<api::Ok<MessageSummaryResponse>, ApiError> {
    Ok(api::Ok(MessageSummaryResponse { no_read_num: 0 }))
}

#[derive(Serialize)]
pub struct CampaignStrategiesResponse {
    strategy: Option<Value>,
}

/// `GET /api/v1/campaign/strategies`.
pub async fn campaign_strategies(
    _user: User,
) -> Result<api::Ok<CampaignStrategiesResponse>, ApiError> {
    Ok(api::Ok(CampaignStrategiesResponse { strategy: None }))
}

#[derive(Serialize)]
pub struct ReleasePopupsResponse {
    popups: [Value; 0],
}

/// `GET /api/v1/release/fetch/popups/info`.
pub async fn release_popups(_user: User) -> Result<api::Ok<ReleasePopupsResponse>, ApiError> {
    Ok(api::Ok(ReleasePopupsResponse { popups: [] }))
}
