//! The `{code, msg, data}` envelope every REST endpoint answers with.

use axum::Json;
use axum::extract::FromRequest;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

/// Success: HTTP 200, `code` 0.
pub struct Ok<T>(pub T);

impl<T: Serialize> IntoResponse for Ok<T> {
    fn into_response(self) -> Response {
        Json(json!({ "code": 0, "msg": "ok", "data": self.0 })).into_response()
    }
}

/// Failure with a wire-level status and code. `msg` stays generic; clients do not read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiError {
    status: StatusCode,
    code: i64,
}

impl ApiError {
    /// The request body is missing, not JSON, or lacks required fields.
    pub const INVALID_REQUEST: Self = Self {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: 1001,
    };

    /// The user token is missing, invalid or expired: clients log in again.
    pub const TOKEN_INVALID: Self = Self {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: 1120,
    };

    /// The QR login is unknown, expired or already used.
    pub const QR_EXPIRED: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: 1190,
    };

    /// `x-param-sign` is missing or wrong. NetEase's code for this is unknown; clients treat any
    /// non-zero code as a rejection.
    pub const INVALID_SIGNATURE: Self = Self {
        status: StatusCode::FORBIDDEN,
        code: 1002,
    };
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "code": self.code, "msg": "error", "data": [] });
        (self.status, Json(body)).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(_: JsonRejection) -> Self {
        Self::INVALID_REQUEST
    }
}

/// JSON request body whose rejection is an [`ApiError`] envelope instead of plain text.
#[derive(FromRequest)]
#[from_request(via(Json), rejection(ApiError))]
pub struct Body<T>(pub T);
