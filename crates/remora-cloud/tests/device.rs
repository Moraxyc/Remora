mod common;

use axum::Router;
use remora_proto::cloud::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct Init {
    device_id: String,
}

const PATH: &str = "/api/v1/device/windows/init";

async fn init(app: &Router, body: Value) -> Result<Init, ApiError> {
    common::post(app, PATH, Some(&body.to_string())).await
}

/// What uu-remote-linux sends, with synthetic values.
fn registration(client_id: &str) -> Value {
    json!({
        "name": "host", "client_id": client_id, "system_id": "SYSTEM", "machine_guid": "guid",
        "os": "Linux", "base_board": "board", "cpu": "cpu", "video": ["gpu"],
        "mac": "AA-BB-CC-DD-EE-FF", "memory": 16384, "screen": "1920x1080",
    })
}

#[tokio::test]
async fn registration_is_stable_per_installation() {
    let app = remora_cloud::router(None);
    let first = init(&app, registration("client-a")).await.unwrap();
    let again = init(&app, registration("client-a")).await.unwrap();
    let other = init(&app, registration("client-b")).await.unwrap();

    assert!(!first.device_id.is_empty());
    assert_eq!(first.device_id, again.device_id);
    assert_ne!(first.device_id, other.device_id);
}

#[tokio::test]
async fn rejects_invalid_bodies_with_an_envelope() {
    let app = remora_cloud::router(None);
    let invalid = Err(ApiError::Rejected {
        http_status: 422,
        code: 1001,
    });

    assert_eq!(
        init(&app, json!({ "name": "host" })).await.map(drop),
        invalid
    );
    assert_eq!(init(&app, registration("")).await.map(drop), invalid);
    let not_json = common::post::<Value>(&app, PATH, Some("{")).await;
    assert_eq!(not_json.map(drop), invalid);
}
