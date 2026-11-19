//! User tokens: HS256 JWTs whose payload carries the `csrf` claim clients read back.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// `{"alg":"HS256","typ":"JWT"}`, base64url-encoded.
const HEADER: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";

/// Secret that signs user tokens. Generated per process for now, so a restart logs everyone out.
pub struct TokenKey([u8; 32]);

impl TokenKey {
    pub fn generate() -> Self {
        Self(random())
    }

    /// A token for `user_id`. Like UU's, it has no `exp`: clients log in again only when it is
    /// rejected.
    pub fn issue(&self, user_id: &str) -> String {
        let claims = Claims {
            sub: user_id,
            csrf: &hex::encode(random::<16>()),
            iat: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
        };
        let payload = serde_json::to_vec(&claims).expect("claims serialize");
        let unsigned = format!("{HEADER}.{}", URL_SAFE_NO_PAD.encode(payload));
        let signature = self.mac(&unsigned).finalize().into_bytes();
        format!("{unsigned}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    /// The user id of a token this key issued, or `None` for anything else.
    pub fn verify(&self, token: &str) -> Option<String> {
        let (unsigned, signature) = token.rsplit_once('.')?;
        let (header, payload) = unsigned.split_once('.')?;
        if header != HEADER {
            return None;
        }
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        self.mac(unsigned).verify_slice(&signature).ok()?;
        let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let claims: VerifiedClaims = serde_json::from_slice(&payload).ok()?;
        Some(claims.sub)
    }

    fn mac(&self, unsigned: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC accepts any key length");
        mac.update(unsigned.as_bytes());
        mac
    }
}

impl fmt::Debug for TokenKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenKey(..)")
    }
}

#[derive(Serialize)]
struct Claims<'a> {
    sub: &'a str,
    csrf: &'a str,
    iat: u64,
}

#[derive(Deserialize)]
struct VerifiedClaims {
    sub: String,
}

/// `N` bytes from the OS random source.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("OS random source available");
    bytes
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn payload(token: &str) -> Value {
        let payload = token.split('.').nth(1).unwrap();
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
    }

    #[test]
    fn issues_signed_jwts_with_a_csrf_claim() {
        let key = TokenKey::generate();
        let token = key.issue("1");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            URL_SAFE_NO_PAD.decode(parts[0]).unwrap(),
            br#"{"alg":"HS256","typ":"JWT"}"#
        );

        let claims = payload(&token);
        assert_eq!(claims["sub"], "1");
        assert_eq!(claims["csrf"].as_str().unwrap().len(), 32);
        assert!(claims.get("exp").is_none());

        let unsigned = format!("{}.{}", parts[0], parts[1]);
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert!(key.mac(&unsigned).verify_slice(&signature).is_ok());
        assert!(
            TokenKey::generate()
                .mac(&unsigned)
                .verify_slice(&signature)
                .is_err()
        );
    }

    #[test]
    fn verifies_only_its_own_untampered_tokens() {
        let key = TokenKey::generate();
        let token = key.issue("1");
        assert_eq!(key.verify(&token).as_deref(), Some("1"));
        assert_eq!(TokenKey::generate().verify(&token), None);

        let parts: Vec<&str> = token.split('.').collect();
        let forged_payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"2","csrf":"x","iat":0}"#);
        let forged = format!("{}.{forged_payload}.{}", parts[0], parts[2]);
        assert_eq!(key.verify(&forged), None);

        let none_alg = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        assert_eq!(key.verify(&format!("{none_alg}.{}.", parts[1])), None);
        assert_eq!(key.verify(&format!("{token}.x")), None);
        assert_eq!(key.verify(""), None);
        assert_eq!(key.verify("a.b"), None);
    }

    #[test]
    fn every_token_gets_its_own_csrf() {
        let key = TokenKey::generate();
        assert_ne!(
            payload(&key.issue("1"))["csrf"],
            payload(&key.issue("1"))["csrf"]
        );
    }

    #[test]
    fn debug_hides_key() {
        assert_eq!(format!("{:?}", TokenKey([7; 32])), "TokenKey(..)");
    }
}
