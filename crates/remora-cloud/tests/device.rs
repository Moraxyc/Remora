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
    let app = remora_cloud::router(common::config());
    let first = init(&app, registration("client-a")).await.unwrap();
    let again = init(&app, registration("client-a")).await.unwrap();
    let other = init(&app, registration("client-b")).await.unwrap();

    assert!(!first.device_id.is_empty());
    assert_eq!(first.device_id, again.device_id);
    assert_ne!(first.device_id, other.device_id);
}

const LIST: &str = "/api/v1/device/list";

#[tokio::test]
async fn the_list_holds_the_users_hosts() {
    let app = remora_cloud::router(common::config());
    let host = common::register_named(&app, "client-host", "studio").await;
    let viewer = common::register_named(&app, "client-viewer", "laptop").await;
    let unnamed = common::register_named(&app, "client-unnamed", "").await;
    let token = common::log_in(&app).await;

    let list: Value = common::get_as(&app, LIST, &token, &viewer).await.unwrap();
    assert_eq!(
        list["my_binded_devices"],
        json!([]),
        "no host has created a room"
    );
    assert_eq!(list["others_shared_devices"], json!([]));
    assert_eq!(list["current_device"]["device_id"], viewer.as_str());

    common::create_room(&app, &token, &host).await;
    common::create_room(&app, &token, &unnamed).await;
    let list: Value = common::get_as(&app, LIST, &token, &viewer).await.unwrap();
    let mut expected = [
        json!({ "device_id": host, "alias": "studio", "platform": 1, "status": "DISCONNECTED" }),
        json!({ "device_id": unnamed, "platform": 1, "status": "DISCONNECTED" }),
    ];
    expected.sort_by_key(|entry| entry["device_id"].as_str().unwrap().to_owned());
    assert_eq!(list["my_binded_devices"], json!(expected));
}

#[tokio::test]
async fn re_registering_renames_the_device() {
    let app = remora_cloud::router(common::config());
    let host = common::register_named(&app, "client-host", "old").await;
    let token = common::log_in(&app).await;
    common::create_room(&app, &token, &host).await;
    assert_eq!(
        common::register_named(&app, "client-host", "new").await,
        host
    );

    let list: Value = common::get_as(&app, LIST, &token, &host).await.unwrap();
    assert_eq!(list["my_binded_devices"][0]["alias"], "new");
}

#[tokio::test]
async fn the_list_needs_a_user_token() {
    let app = remora_cloud::router(common::config());
    let result = common::get_as::<Value>(&app, LIST, "bogus", "device").await;
    assert_eq!(result.map(drop), Err(ApiError::TokenExpired));
}

#[tokio::test]
async fn rejects_invalid_bodies_with_an_envelope() {
    let app = remora_cloud::router(common::config());
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
