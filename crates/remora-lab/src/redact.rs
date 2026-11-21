//! Secret redaction and leak detection for lab traces and target logs.

use serde_json::{Map, Value};

const REDACTED: &str = "<redacted>";

fn is_secret_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "token"
            | "qrcode_jump_url"
            | "status_query_ticket"
            | "reconnect_key"
            | "report_token"
            | "credential"
            | "password"
            | "authorization"
            | "x-nrd-auth"
            | "x-param-sign"
            | "sign_key"
            | "turn_secret"
            | "iceservers"
    )
}

/// Returns a redacted copy of `value`, scrubbing credential fields and SDP secrets.
pub fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_map(map)),
        Value::Array(arr) => Value::Array(arr.iter().map(redact_value).collect()),
        other => other.clone(),
    }
}

/// Returns a redacted copy of `map`.
pub fn redact_map(map: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::with_capacity(map.len());
    for (k, v) in map {
        if is_secret_key(k) {
            out.insert(k.clone(), Value::String(REDACTED.into()));
        } else if k.eq_ignore_ascii_case("sdp")
            && let Some(sdp) = v.as_str()
        {
            out.insert(k.clone(), Value::String(redact_sdp(sdp)));
        } else {
            out.insert(k.clone(), redact_value(v));
        }
    }
    out
}

/// Scrubs `ice-ufrag`, `ice-pwd`, and `fingerprint` attributes inside an SDP blob.
pub fn redact_sdp(sdp: &str) -> String {
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        if line.starts_with("a=ice-ufrag:") {
            out.push_str("a=ice-ufrag:<redacted>\r\n");
        } else if line.starts_with("a=ice-pwd:") {
            out.push_str("a=ice-pwd:<redacted>\r\n");
        } else if line.starts_with("a=fingerprint:") {
            out.push_str("a=fingerprint:<redacted>\r\n");
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Scans `haystack` (e.g. captured server logs or JSONL trace) for any literal secret in `secrets`.
/// Returns the first leaked secret index/description without echoing the secret itself.
pub fn find_secret_leak(haystack: &str, secrets: &[(String, String)]) -> Option<String> {
    for (label, secret) in secrets {
        if secret.len() >= 4 && haystack.contains(secret.as_str()) {
            return Some(label.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn redacts_tokens_and_sdp_secrets() {
        let input = json!({
            "token": "secret-jwt",
            "sdp": "v=0\r\na=ice-ufrag:u123\r\na=ice-pwd:p456\r\na=fingerprint:sha-256 AA:BB\r\n",
            "nested": { "reconnect_key": "rk-1", "device_id": "d-1" }
        });
        let out = redact_value(&input);
        let serialized = out.to_string();
        assert!(!serialized.contains("secret-jwt"));
        assert!(!serialized.contains("u123"));
        assert!(!serialized.contains("p456"));
        assert!(!serialized.contains("AA:BB"));
        assert!(!serialized.contains("rk-1"));
        assert!(serialized.contains("d-1"));
    }

    #[test]
    fn detects_leaks_without_echoing_secret() {
        let secrets = vec![("user_token".into(), "jwt.super.secret".into())];
        assert_eq!(
            find_secret_leak("INFO connected token=jwt.super.secret", &secrets),
            Some("user_token".into())
        );
        assert_eq!(find_secret_leak("INFO connected user_id=1", &secrets), None);
    }
}
