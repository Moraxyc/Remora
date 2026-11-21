//! Cloud REST API request signing.

use std::fmt;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

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
    hex::encode(
        mac(key, method, path, headers, body)
            .finalize()
            .into_bytes(),
    )
}

/// Whether `signature`, a hex [`SIGN_HEADER`] value, matches the request as [`sign`] would sign
/// it. Compares in constant time.
pub fn verify<'a>(
    key: &SignKey,
    method: &str,
    path: &str,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    body: &[u8],
    signature: &str,
) -> bool {
    hex::decode(signature).is_ok_and(|tag| {
        mac(key, method, path, headers, body)
            .verify_slice(&tag)
            .is_ok()
    })
}

fn mac<'a>(
    key: &SignKey,
    method: &str,
    path: &str,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    body: &[u8],
) -> Hmac<Sha256> {
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
    mac
}

/// Computes the lowercase-hex `SHA-256(salt || code)` digest used by remote-assist
/// `upload/sign` and `upload_sign` challenges.
pub fn share_code_digest(salt: &str, code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(code.as_bytes());
    hex::encode(hasher.finalize())
}

/// Checks in constant time whether `signature_hex` equals `SHA-256(salt || code)`.
pub fn verify_share_code_digest(salt: &str, code: &str, signature_hex: &str) -> bool {
    let Ok(provided) = hex::decode(signature_hex) else {
        return false;
    };
    if provided.len() != 32 {
        return false;
    }
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(code.as_bytes());
    let expected = hasher.finalize();
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(provided.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

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
    fn verifies_own_signatures_only() {
        let headers = [("x-param-plat", "1"), ("x-param-ts", "1700000000")];
        let signature = sign(&key(), "POST", "/p", headers, b"{}");
        let check = |key: &SignKey, body: &[u8], signature: &str| {
            verify(key, "POST", "/p", headers, body, signature)
        };

        assert!(check(&key(), b"{}", &signature));
        assert!(check(&key(), b"{}", &signature.to_ascii_uppercase()));
        assert!(!check(&key(), b"{ }", &signature));
        assert!(!check(&SignKey::new("other"), b"{}", &signature));
        assert!(!check(&key(), b"{}", &signature[..62]));
        assert!(!check(&key(), b"{}", "not hex"));
        assert!(!check(&key(), b"{}", ""));
    }

    #[test]
    fn debug_hides_key() {
        assert!(!format!("{:?}", key()).contains("test"));
    }

    #[test]
    fn share_code_digest_computes_and_verifies_sha256() {
        let digest = share_code_digest("salt-1", "CODE1234");
        assert_eq!(digest.len(), 64);
        assert!(verify_share_code_digest("salt-1", "CODE1234", &digest));
        assert!(verify_share_code_digest(
            "salt-1",
            "CODE1234",
            &digest.to_ascii_uppercase()
        ));
        assert!(!verify_share_code_digest("salt-1", "WRONG", &digest));
        assert!(!verify_share_code_digest("salt-2", "CODE1234", &digest));
        assert!(!verify_share_code_digest("salt-1", "CODE1234", "bad"));
        assert!(!verify_share_code_digest(
            "salt-1",
            "CODE1234",
            &digest[..62]
        ));
    }
}
