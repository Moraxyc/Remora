#![allow(dead_code, reason = "each test binary uses a different subset")]

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::http::request::Builder;
use http_body_util::BodyExt;
use remora_cloud::{Config, Database, Ping, ReconnectGrace};
use remora_proto::cloud::{ApiError, parse_response};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tower::ServiceExt;

pub const PUBLIC_URL: &str = "https://remora.test";

/// Signatures unchecked, logins confirmed automatically.
pub fn config() -> Config {
    Config {
        public_url: PUBLIC_URL.parse().unwrap(),
        sign_key: None,
        ping: Ping::default(),
        reconnect_grace: ReconnectGrace::default(),
        ice: Default::default(),
        database: Database::in_memory(),
        auto_confirm: true,
        owner_password: None,
        oidc: None,
    }
}

/// POSTs `body` as JSON, or an empty body like clients do for calls without parameters, and
/// decodes the answer the way clients do.
pub async fn post<T: DeserializeOwned>(
    app: &Router,
    path: &str,
    body: Option<&str>,
) -> Result<T, ApiError> {
    let mut request = Request::post(path);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    send(app, request, body.unwrap_or_default()).await
}

/// GETs `path`, authenticated with `token` the way clients send a user token.
pub async fn get<T: DeserializeOwned>(
    app: &Router,
    path: &str,
    token: Option<&str>,
) -> Result<T, ApiError> {
    let mut request = Request::get(path);
    if let Some(token) = token {
        request = authenticated(request, token);
    }
    send(app, request, "").await
}

/// GETs `path` as the user of `token`, from the registered device `device_id`.
pub async fn get_as<T: DeserializeOwned>(
    app: &Router,
    path: &str,
    token: &str,
    device_id: &str,
) -> Result<T, ApiError> {
    let request = authenticated(Request::get(path), token).header("x-param-device-id", device_id);
    send(app, request, "").await
}

/// POSTs a JSON `body` as the user of `token`, from the registered device `device_id`.
pub async fn post_as<T: DeserializeOwned>(
    app: &Router,
    path: &str,
    token: &str,
    device_id: Option<&str>,
    body: &str,
) -> Result<T, ApiError> {
    let mut request =
        authenticated(Request::post(path), token).header("content-type", "application/json");
    if let Some(device_id) = device_id {
        request = request.header("x-param-device-id", device_id);
    }
    send(app, request, body).await
}

fn authenticated(request: Builder, token: &str) -> Builder {
    request
        .header("authorization", format!("Bearer {token}"))
        .header(
            "cookie",
            format!("nrd_access_token={token}; nrd_csrf_access_token=c"),
        )
        .header("x-csrf-token", "c")
}

/// Registers an installation and returns its device id.
pub async fn register(app: &Router, client_id: &str) -> String {
    register_named(app, client_id, "host").await
}

pub async fn register_named(app: &Router, client_id: &str, name: &str) -> String {
    let body = json!({ "name": name, "client_id": client_id });
    let init: Value = post(app, "/api/v1/device/windows/init", Some(&body.to_string()))
        .await
        .unwrap();
    init["device_id"].as_str().unwrap().to_owned()
}

async fn send<T: DeserializeOwned>(
    app: &Router,
    request: Builder,
    body: &str,
) -> Result<T, ApiError> {
    let request = request.body(Body::from(body.to_owned())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    parse_response(status, &bytes)
}

/// Creates a room for a host on `device_id`, binding the device to the user, and returns the
/// room token.
pub async fn create_room(app: &Router, token: &str, device_id: &str) -> String {
    let room: Value = post_as(app, "/api/v1/room/create", token, Some(device_id), "{}")
        .await
        .unwrap();
    room["token"].as_str().unwrap().to_owned()
}

/// Runs the whole QR login and returns the user token.
pub async fn log_in(app: &Router) -> String {
    let qr: Value = post(app, "/api/v1/qrcode/gen/login", None).await.unwrap();
    let body = json!({
        "token": qr["status_query_ticket"],
        "qrcode_jump_url": qr["qrcode_jump_url"],
    });
    let login: Value = post(app, "/api/v1/login/by_qrcode", Some(&body.to_string()))
        .await
        .unwrap();
    login["token"].as_str().unwrap().to_owned()
}
