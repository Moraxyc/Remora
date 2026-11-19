mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use remora_proto::cloud::ApiError;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Room {
    signaling_server: String,
    signaling_list: Vec<String>,
    token: String,
    ws_connect_timeout_ms: u32,
}

const PATH: &str = "/api/v1/room/create";

async fn create(app: &Router, token: &str, device_id: Option<&str>) -> Result<Room, ApiError> {
    common::post_as(app, PATH, token, device_id, "{}").await
}

#[tokio::test]
async fn a_logged_in_host_gets_signaling_on_this_server() {
    let app = remora_cloud::router(common::config());
    let device_id = common::register(&app, "client-a").await;
    let token = common::log_in(&app).await;

    let room = create(&app, &token, Some(&device_id)).await.unwrap();
    assert_eq!(room.signaling_server, "wss://remora.test");
    assert_eq!(room.signaling_list, ["wss://remora.test"]);
    assert!(room.ws_connect_timeout_ms > 0);

    // Clients read the room token's lifetime from its `exp` claim.
    let payload = room.token.split('.').nth(1).unwrap();
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(claims["exp"].as_u64().unwrap() > now);
}

#[tokio::test]
async fn needs_a_user_token() {
    let app = remora_cloud::router(common::config());
    let device_id = common::register(&app, "client-a").await;

    let result = create(&app, "bogus", Some(&device_id)).await;
    assert_eq!(result.map(drop), Err(ApiError::TokenExpired));
}

#[tokio::test]
async fn needs_a_registered_device() {
    let app = remora_cloud::router(common::config());
    let token = common::log_in(&app).await;
    let invalid = Err(ApiError::Rejected {
        http_status: 422,
        code: 1001,
    });

    assert_eq!(create(&app, &token, None).await.map(drop), invalid);
    assert_eq!(
        create(&app, &token, Some("unknown")).await.map(drop),
        invalid
    );
}
