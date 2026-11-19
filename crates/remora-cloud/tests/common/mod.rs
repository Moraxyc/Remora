use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use remora_proto::cloud::{ApiError, parse_response};
use serde::de::DeserializeOwned;
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
    let body = Body::from(body.unwrap_or_default().to_owned());
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    parse_response(status, &bytes)
}
