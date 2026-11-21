//! JSON-RPC 2.0 endpoint driver protocol over stdio (`newline-delimited JSON`).

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Num(u64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriverRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: DriverMethod,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriverResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<DriverRpcError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriverRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriverNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DriverMethod {
    Initialize,
    RegisterDevice,
    Login,
    RegisterHost,
    Connect,
    SendControl,
    SendSoac,
    SendInput,
    PushVideoFrame,
    CloseRoom,
    Shutdown,
}

impl DriverResponse {
    pub fn ok(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: RequestId, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: None,
            error: Some(DriverRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Reads one newline-delimited JSON-RPC request from `reader`. Returns `Ok(None)` on EOF.
pub fn read_request<R: BufRead>(reader: &mut R) -> io::Result<Option<DriverRequest>> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req = serde_json::from_str::<DriverRequest>(trimmed)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        return Ok(Some(req));
    }
}

/// Writes one newline-delimited JSON-RPC message (response or notification) to `writer`.
pub fn write_message<W: Write, T: Serialize>(writer: &mut W, msg: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, msg).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::*;

    #[test]
    fn roundtrips_jsonrpc_driver_requests_and_responses() {
        let req = DriverRequest {
            jsonrpc: JSONRPC_VERSION.into(),
            id: RequestId::Num(1),
            method: DriverMethod::Initialize,
            params: json!({
                "base_url": "http://127.0.0.1:8680",
                "profile": "windows-4.48.2",
                "role": "host"
            }),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &req).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = read_request(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded, req);
        assert!(read_request(&mut cursor).unwrap().is_none());

        let resp = DriverResponse::ok(RequestId::Num(1), json!({ "ready": true }));
        let mut out = Vec::new();
        write_message(&mut out, &resp).unwrap();
        let parsed: DriverResponse = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed, resp);
    }
}
