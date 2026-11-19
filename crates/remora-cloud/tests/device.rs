use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use remora_proto::cloud::{ApiError, parse_response};
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;

#[derive(Debug, Deserialize)]
struct Init {
    device_id: String,
}

async fn post(app: &Router, path: &str, body: &str) -> Result<Init, ApiError> {
    let request = Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    parse_response(status, &bytes)
}

async fn init(app: &Router, body: Value) -> Result<Init, ApiError> {
    post(app, "/api/v1/device/windows/init", &body.to_string()).await
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
    let app = remora_cloud::router(Default::default());
    let first = init(&app, registration("client-a")).await.unwrap();
    let again = init(&app, registration("client-a")).await.unwrap();
    let other = init(&app, registration("client-b")).await.unwrap();

    assert!(!first.device_id.is_empty());
    assert_eq!(first.device_id, again.device_id);
    assert_ne!(first.device_id, other.device_id);
}

#[tokio::test]
async fn rejects_invalid_bodies_with_an_envelope() {
    let app = remora_cloud::router(Default::default());
    let invalid = Err(ApiError::Rejected {
        http_status: 422,
        code: 1001,
    });

    assert_eq!(
        init(&app, json!({ "name": "host" })).await.map(drop),
        invalid
    );
    assert_eq!(init(&app, registration("")).await.map(drop), invalid);
    let not_json = post(&app, "/api/v1/device/windows/init", "{").await;
    assert_eq!(not_json.map(drop), invalid);
}
