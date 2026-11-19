//! Cloud REST API: request signing and the response envelope.

use std::fmt;

use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::Sha256;

/// Headers with this prefix are covered by the signature.
pub const PARAM_PREFIX: &str = "x-param-";
/// Carries the signature; excluded from what it signs.
pub const SIGN_HEADER: &str = "x-param-sign";

/// HMAC key shared by the cloud and all its clients; whoever runs the cloud chooses it.
#[derive(Clone)]
pub struct SignKey(Vec<u8>);

impl SignKey {
    pub fn new(key: impl Into<Vec<u8>>) -> Self {
        Self(key.into())
    }
}

impl fmt::Debug for SignKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignKey(..)")
    }
}

/// Value for [`SIGN_HEADER`]: lowercase hex HMAC-SHA256 over
/// `METHOD + path + sorted "name=value" params joined by "&" + body`.
///
/// `headers` may contain any headers; only `x-param-*` ones are signed, with names lowercased.
/// `path` is exactly as sent, including the query string. `body` is the exact bytes sent:
/// an empty body and `{}` sign differently.
pub fn sign<'a>(
    key: &SignKey,
    method: &str,
    path: &str,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    body: &[u8],
) -> String {
    let mut params: Vec<(String, &str)> = headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value))
        .filter(|(name, _)| name.starts_with(PARAM_PREFIX) && name != SIGN_HEADER)
        .collect();
    params.sort_unstable();

    let mut mac = Hmac::<Sha256>::new_from_slice(&key.0).expect("HMAC accepts any key length");
    mac.update(method.to_ascii_uppercase().as_bytes());
    mac.update(path.as_bytes());
    for (i, (name, value)) in params.iter().enumerate() {
        if i > 0 {
            mac.update(b"&");
        }
        mac.update(name.as_bytes());
        mac.update(b"=");
        mac.update(value.as_bytes());
    }
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

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

    fn key() -> SignKey {
        SignKey::new("test-sign-key")
    }

    // Expected values computed independently with `openssl dgst -sha256 -hmac test-sign-key`.
    #[test]
    fn signs_sorted_lowercased_params_and_body() {
        let headers = [
            ("X-Param-TS", "1700000000"),
            ("x-param-plat", "1"),
            ("Authorization", "Bearer t"),
            ("X-Param-SIGN", "stale"),
            ("x-param-chn", "gwqd"),
        ];
        assert_eq!(
            sign(&key(), "post", "/api/v1/room/create", headers, b"{}"),
            "62856610dd4ccad2c56b434460347d335e41ffcf7e963e5c5e409dfca077e473"
        );
    }

    #[test]
    fn signs_query_string_and_empty_body() {
        let headers = [("x-param-plat", "1")];
        assert_eq!(
            sign(
                &key(),
                "GET",
                "/api/v1/tool/fp/token?filetype=1",
                headers,
                b""
            ),
            "a577257f61e702192f237f5292022ce51f27caeb56b2b611185599d23eab30d4"
        );
    }

    #[test]
    fn empty_and_braces_bodies_differ() {
        let sign_body = |body| sign(&key(), "POST", "/p", [("x-param-plat", "1")], body);
        assert_ne!(sign_body(b""), sign_body(b"{}"));
    }

    #[test]
    fn debug_hides_key() {
        assert!(!format!("{:?}", key()).contains("test"));
    }
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
