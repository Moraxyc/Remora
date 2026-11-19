#![allow(dead_code, reason = "each test binary uses a different subset")]

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::http::request::Builder;
use http_body_util::BodyExt;
use remora_proto::cloud::{ApiError, parse_response};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tower::ServiceExt;

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
        request = request
            .header("authorization", format!("Bearer {token}"))
            .header(
                "cookie",
                format!("nrd_access_token={token}; nrd_csrf_access_token=c"),
            )
            .header("x-csrf-token", "c");
    }
    send(app, request, "").await
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
