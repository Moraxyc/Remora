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

/// Failure with a wire-level status and code. Some clients show `msg` to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiError {
    status: StatusCode,
    code: i64,
    msg: &'static str,
}

impl ApiError {
    /// The request body is missing, not JSON, or lacks required fields.
    pub const INVALID_REQUEST: Self = Self {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: 1001,
        msg: "invalid request",
    };

    /// The user token is missing, invalid or expired: clients log in again.
    pub const TOKEN_INVALID: Self = Self {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: 1120,
        msg: "login expired",
    };

    /// The QR login is unknown, expired or already used.
    pub const QR_EXPIRED: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: 1190,
        msg: "QR code expired",
    };

    /// `x-param-sign` is missing or wrong. NetEase's code for this is unknown; clients treat any
    /// non-zero code as a rejection.
    pub const INVALID_SIGNATURE: Self = Self {
        status: StatusCode::FORBIDDEN,
        code: 1002,
        msg: "invalid signature",
    };

    /// The device is not one of the user's. This code and the next are Remora's own.
    pub const UNKNOWN_DEVICE: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: 2003,
        msg: "no such device",
    };

    /// Another device controls the host's desktop. Clients know this code and say so.
    pub const BEING_CONTROLLED: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: 2002,
        msg: "被控端正在被远控中，无法发起连接",
    };

    /// The device's host is not on signaling.
    pub const DEVICE_OFFLINE: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: 2001,
        msg: "device is offline",
    };
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "code": self.code, "msg": self.msg, "data": {} });
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
