//! Optional `x-param-sign` verification.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use remora_proto::control::{SIGN_HEADER, SignKey, verify};

use crate::api::ApiError;

/// Request bodies are small JSON objects; anything larger is not a client of ours.
const MAX_BODY: usize = 1024 * 1024;

/// Rejects requests whose signature does not match. The body has to be read to check it, so it
/// is buffered and handed on unchanged.
pub async fn require_signature(
    State(key): State<Arc<SignKey>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(body) = to_bytes(body, MAX_BODY).await else {
        return ApiError::INVALID_REQUEST.into_response();
    };
    let headers = parts
        .headers
        .iter()
        .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)));
    let signature = parts
        .headers
        .get(SIGN_HEADER)
        .and_then(|value| value.to_str().ok());
    let path = parts.uri.path_and_query().map_or("/", |path| path.as_str());

    let valid = signature.is_some_and(|signature| {
        verify(&key, parts.method.as_str(), path, headers, &body, signature)
    });
    if !valid {
        tracing::warn!(method = %parts.method, path = parts.uri.path(), "bad request signature");
        return ApiError::INVALID_SIGNATURE.into_response();
    }
    next.run(Request::from_parts(parts, Body::from(body))).await
}
