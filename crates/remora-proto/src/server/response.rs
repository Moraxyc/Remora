//! Stateless `{code, msg, data}` response envelope builders.

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;

/// Empty JSON object `{}` used as `data` when an endpoint returns no fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyData {}

/// Wire response envelope `{code, msg, data}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResponseEnvelope<'a, T> {
    pub code: i64,
    pub msg: &'a str,
    pub data: T,
}

/// Builds a successful `{code: 0, msg: "ok", data}` envelope.
pub fn ok_envelope<T>(data: T) -> ResponseEnvelope<'static, T> {
    ResponseEnvelope {
        code: ErrorCode::Ok.code(),
        msg: "ok",
        data,
    }
}

/// Builds an error `{code, msg, data: {}}` envelope.
pub fn error_envelope(code: ErrorCode, msg: &str) -> ResponseEnvelope<'_, EmptyData> {
    ResponseEnvelope {
        code: code.code(),
        msg,
        data: EmptyData {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_envelopes() {
        let ok = serde_json::to_value(ok_envelope(EmptyData {})).unwrap();
        assert_eq!(
            ok,
            serde_json::json!({ "code": 0, "msg": "ok", "data": {} })
        );

        let err =
            serde_json::to_value(error_envelope(ErrorCode::InvalidSignature, "bad sign")).unwrap();
        assert_eq!(
            err,
            serde_json::json!({ "code": 1002, "msg": "bad sign", "data": {} })
        );
    }
}
