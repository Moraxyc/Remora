//! Stateless HTTP request signature verification for the cloud control plane.

use std::fmt;

use crate::control::signing::{SIGN_HEADER, SignKey, verify};

/// Why a signed control-plane request failed signature verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VerifyError {
    MissingSignature,
    InvalidSignature,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSignature => f.write_str("missing x-param-sign header"),
            Self::InvalidSignature => f.write_str("request signature mismatch"),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Extracts [`SIGN_HEADER`] (case-insensitively) from `headers` and verifies the HMAC-SHA256
/// signature against `key`, `method`, `path`, the remaining `x-param-*` headers, and `body`.
pub fn verify_request<'a>(
    key: &SignKey,
    method: &str,
    path: &str,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    body: &[u8],
) -> Result<(), VerifyError> {
    let mut collected = Vec::new();
    let mut signature = None;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(SIGN_HEADER) {
            if signature.is_none() {
                signature = Some(value);
            }
        } else {
            collected.push((name, value));
        }
    }
    let signature = signature.ok_or(VerifyError::MissingSignature)?;
    if verify(key, method, path, collected, body, signature) {
        Ok(())
    } else {
        Err(VerifyError::InvalidSignature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::sign;

    #[test]
    fn verifies_and_rejects_requests() {
        let key = SignKey::new("test-key");
        let headers = [
            ("X-Param-Plat", "4"),
            ("X-Param-VN", "4.48.2"),
            ("X-Param-TS", "1700000000"),
        ];
        let sig = sign(&key, "POST", "/api/v1/device/macos/init", headers, b"{}");

        let mut signed = headers.to_vec();
        signed.push(("X-Param-SIGN", &sig));
        assert_eq!(
            verify_request(&key, "POST", "/api/v1/device/macos/init", signed, b"{}"),
            Ok(())
        );

        assert_eq!(
            verify_request(&key, "POST", "/api/v1/device/macos/init", headers, b"{}"),
            Err(VerifyError::MissingSignature)
        );

        let mut bad = headers.to_vec();
        bad.push(("x-param-sign", "deadbeef"));
        assert_eq!(
            verify_request(&key, "POST", "/api/v1/device/macos/init", bad, b"{}"),
            Err(VerifyError::InvalidSignature)
        );
    }
}
