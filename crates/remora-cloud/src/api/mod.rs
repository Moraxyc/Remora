//! The `{code, msg, data}` envelope every REST endpoint answers with.

pub(crate) mod address;
pub(crate) mod auth;
pub(crate) mod compat;
pub(crate) mod device;
pub(crate) mod files;
pub(crate) mod middleware;
pub(crate) mod room;
pub(crate) mod share;
pub(crate) mod user;
pub(crate) mod wake;
pub(crate) mod web;

use axum::Json;
use axum::body::Bytes;
use axum::extract::FromRequest;
use axum::extract::Request;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use remora_proto::error::ErrorCode;
use remora_proto::server::{EmptyData, error_envelope, ok_envelope};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Success: HTTP 200, `code` 0.
pub struct Ok<T>(pub T);

/// `data` of a success that has nothing to say: `{}`.
pub type Empty = EmptyData;

impl<T: Serialize> IntoResponse for Ok<T> {
    fn into_response(self) -> Response {
        Json(ok_envelope(self.0)).into_response()
    }
}

/// Failure with a wire-level status and code. Some clients show `msg` to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiError {
    status: StatusCode,
    code: ErrorCode,
    msg: &'static str,
}

impl ApiError {
    /// The request body is missing, not JSON, or lacks required fields.
    pub const INVALID_REQUEST: Self = Self {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: ErrorCode::InvalidRequest,
        msg: "invalid request",
    };

    /// The user token is missing, invalid or expired: clients log in again.
    pub const TOKEN_INVALID: Self = Self {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: ErrorCode::TokenExpired,
        msg: "login expired",
    };

    pub const CLIENT_UP_TO_DATE: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::ClientUpToDate,
        msg: "当前已是最新版本",
    };

    pub const CONFIG_UP_TO_DATE: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::ConfigUpToDate,
        msg: "当前配置已是最新版本",
    };

    /// The QR login is unknown, expired or already used.
    pub const QR_EXPIRED: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::QrCodeExpired,
        msg: "QR code expired",
    };

    /// `x-param-sign` is missing or wrong. NetEase's code for this is unknown; clients treat any
    /// non-zero code as a rejection.
    pub const INVALID_SIGNATURE: Self = Self {
        status: StatusCode::FORBIDDEN,
        code: ErrorCode::InvalidSignature,
        msg: "invalid signature",
    };

    /// The device is not one of the user's. This code and the next are Remora's own.
    pub const UNKNOWN_DEVICE: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::UnknownDevice,
        msg: "no such device",
    };

    /// Another device controls the host's desktop. Clients know this code and say so.
    pub const BEING_CONTROLLED: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::BeingControlled,
        msg: "被控端正在被远控中，无法发起连接",
    };

    /// Something on the server failed, such as the database. Remora's own code.
    pub const INTERNAL: Self = Self {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: ErrorCode::Internal,
        msg: "internal error",
    };

    /// The device's host is not on signaling.
    pub const DEVICE_OFFLINE: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::DeviceOffline,
        msg: "device is offline",
    };

    /// The client tried too often and has to wait. UU's own code for this is not known;
    /// clients treat it like any other failure.
    pub const TOO_MANY_REQUESTS: Self = Self {
        status: StatusCode::TOO_MANY_REQUESTS,
        code: ErrorCode::TooManyRequests,
        msg: "too many requests",
    };

    /// Guest share info requested before creating a guest room.
    pub const GUEST_ROOM_REQUIRED: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::GuestRoomRequired,
        msg: "guest room required",
    };

    /// Same-account remote assistance is refused.
    pub const SAME_ACCOUNT_ASSIST: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::CannotInitiateRemoteConnection,
        msg: "无法对同账号的设备发起远程协助",
    };

    /// The target host has not enabled remote assistance.
    pub const ASSIST_DISABLED: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::CannotInitiateRemoteConnection,
        msg: "对方未开启协助",
    };

    /// Incorrect share device ID or verification code.
    pub const INCORRECT_SHARE_CODE: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::IncorrectShareCode,
        msg: "验证码错误",
    };

    /// Confirmation was denied or cancelled by the target host.
    pub const SHARE_CONFIRMATION_DENIED: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::ShareConfirmationDenied,
        msg: "对方已拒绝远程协助",
    };

    /// Confirmation is still pending on the target host.
    pub const SHARE_CONFIRMATION_REQUIRED: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::ShareConfirmationRequired,
        msg: "等待对方确认",
    };

    /// Cannot favorite a same-account device in share favorites.
    pub const SAME_ACCOUNT_SHARE_FAVORITE: Self = Self {
        status: StatusCode::BAD_REQUEST,
        code: ErrorCode::SameAccountShareFavorite,
        msg: "无法对同账号的设备发起远程协助",
    };
}

impl ApiError {
    pub fn into_response_with_data<T: Serialize>(self, data: T) -> Response {
        (
            self.status,
            Json(remora_proto::server::ResponseEnvelope {
                code: self.code.code(),
                msg: self.msg,
                data,
            }),
        )
            .into_response()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(error_envelope(self.code, self.msg))).into_response()
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(error: rusqlite::Error) -> Self {
        tracing::error!(%error, "database query failed");
        Self::INTERNAL
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

/// JSON request body that also accepts an empty body (`""` / whitespace) as `T::default()`.
pub struct OptionalBody<T>(pub T);

impl<S, T> FromRequest<S> for OptionalBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Default,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|_| ApiError::INVALID_REQUEST)?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return std::result::Result::Ok(Self(T::default()));
        }
        serde_json::from_slice(&bytes)
            .map(Self)
            .map_err(|_| ApiError::INVALID_REQUEST)
    }
}
