//! Cloud REST API response envelope and errors.

use std::fmt;

use serde::Deserialize;
use serde::de::DeserializeOwned;

/// Result of one cloud call, from the HTTP status and the `{code, msg, data}` body.
///
/// Success requires HTTP 200 *and* `code == 0`. `msg` is never kept: for auth calls it can
/// contain tickets.
pub fn parse_response<T: DeserializeOwned>(http_status: u16, body: &[u8]) -> Result<T, ApiError> {
    let envelope: Envelope =
        serde_json::from_slice(body).map_err(|_| ApiError::Malformed { http_status })?;
    match (http_status, envelope.code) {
        (200, 0) => T::deserialize(envelope.data).map_err(|_| ApiError::Malformed { http_status }),
        (_, 1120) => Err(ApiError::TokenExpired),
        (_, 1190) => Err(ApiError::QrCodeExpired),
        (_, code) => Err(ApiError::Rejected { http_status, code }),
    }
}

#[derive(Deserialize)]
struct Envelope {
    code: i64,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApiError {
    /// The user token is invalid or expired: log in again, do not retry.
    TokenExpired,
    /// The login QR code expired: generate a new one.
    QrCodeExpired,
    /// Any other non-zero code, e.g. 1001 for an invalid request body.
    Rejected { http_status: u16, code: i64 },
    /// The body is not an envelope, or `data` has an unexpected shape.
    Malformed { http_status: u16 },
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TokenExpired => f.write_str("login expired"),
            Self::QrCodeExpired => f.write_str("QR code expired"),
            Self::Rejected { http_status, code } => {
                write!(
                    f,
                    "cloud rejected the request: HTTP {http_status}, code {code}"
                )
            }
            Self::Malformed { http_status } => {
                write!(f, "malformed cloud response: HTTP {http_status}")
            }
        }
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Deserialize)]
    struct Room {
        token: String,
    }

    #[test]
    fn parses_success_data() {
        let body = br#"{"code":0,"msg":"ok","data":{"token":"t","extra":1}}"#;
        assert_eq!(parse_response(200, body), Ok(Room { token: "t".into() }));
        assert_eq!(parse_response(200, br#"{"code":0,"msg":"ok"}"#), Ok(()));
    }

    #[test]
    fn classifies_errors() {
        let token = r#"{"code":1120,"msg":"Token无效或已过期"}"#.as_bytes();
        assert_eq!(
            parse_response::<()>(422, token),
            Err(ApiError::TokenExpired)
        );
        let qr = br#"{"code":1190,"msg":"expired"}"#;
        assert_eq!(parse_response::<()>(400, qr), Err(ApiError::QrCodeExpired));
        let invalid = br#"{"code":1001,"msg":"","data":[{"loc":["body"]}]}"#;
        assert_eq!(
            parse_response::<()>(422, invalid),
            Err(ApiError::Rejected {
                http_status: 422,
                code: 1001
            })
        );
        assert_eq!(
            parse_response::<()>(500, br#"{"code":0}"#),
            Err(ApiError::Rejected {
                http_status: 500,
                code: 0
            })
        );
    }

    #[test]
    fn rejects_malformed_bodies() {
        assert_eq!(
            parse_response::<()>(502, b"<html>"),
            Err(ApiError::Malformed { http_status: 502 })
        );
        assert_eq!(
            parse_response::<Room>(200, br#"{"code":0,"data":{}}"#),
            Err(ApiError::Malformed { http_status: 200 })
        );
    }
}
