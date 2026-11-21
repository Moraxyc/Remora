//! JSONL wire trace recorder conforming to `compat/schema/trace.schema.json` (v0).

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const SCHEMA_VERSION: &str = "v0";

/// Protocol plane of a recorded wire item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TracePlane {
    Control,
    Signal,
    Session,
}

/// Direction of a recorded wire item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TraceDirection {
    In,
    Out,
    C2s,
    S2c,
    V2h,
    H2v,
}

/// Endpoint role that produced the trace entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TraceActor {
    Host,
    Viewer,
    Cloud,
}

/// Kind of wire event recorded in one JSONL line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TraceKind {
    HttpRequest,
    HttpResponse,
    WsText,
    WsBinary,
    DcMessage,
    RtpExt,
    Timeout,
    Event,
}

/// One JSONL record matching `compat/schema/trace.schema.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceRecord {
    pub schema_version: String,
    pub seq: u64,
    pub ts_ms: u64,
    pub plane: TracePlane,
    pub direction: TraceDirection,
    pub actor: TraceActor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub kind: TraceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<Value>,
    pub redacted: bool,
}

/// Unnumbered event payload passed to [`TraceRecorder::record`].
#[derive(Debug, Clone)]
pub struct TraceEvent {
    pub plane: TracePlane,
    pub direction: TraceDirection,
    pub channel: Option<String>,
    pub kind: TraceKind,
    pub summary: Option<Map<String, Value>>,
    pub payload_bytes: Option<Vec<u8>>,
    pub payload_json: Option<Value>,
}

struct RecorderInner {
    writer: Box<dyn Write + Send>,
    seq: u64,
}

/// Thread-safe JSONL trace writer that timestamps, numbers, and redacts endpoint traffic.
pub struct TraceRecorder {
    actor: TraceActor,
    start: Instant,
    redact: bool,
    inner: Mutex<RecorderInner>,
}

impl std::fmt::Debug for TraceRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceRecorder")
            .field("actor", &self.actor)
            .field("redact", &self.redact)
            .finish()
    }
}

impl TraceRecorder {
    /// Creates a recorder writing JSONL lines to `writer`. Redaction is enabled by default.
    pub fn new(actor: TraceActor, writer: impl Write + Send + 'static) -> Self {
        Self {
            actor,
            start: Instant::now(),
            redact: true,
            inner: Mutex::new(RecorderInner {
                writer: Box::new(writer),
                seq: 0,
            }),
        }
    }

    /// Creates or truncates `path` and writes JSONL trace lines into a buffered file writer.
    pub fn create(actor: TraceActor, path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self::new(actor, BufWriter::new(file)))
    }

    /// Configures whether secret fields are scrubbed before serialization.
    pub fn with_redaction(mut self, redact: bool) -> Self {
        self.redact = redact;
        self
    }

    /// Appends one JSONL line and flushes the underlying writer.
    pub fn record(&self, event: TraceEvent) -> io::Result<()> {
        let ts_ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let payload_json = event
            .payload_json
            .map(|v| if self.redact { redact_value(&v) } else { v });
        let summary = event
            .summary
            .map(|s| if self.redact { redact_map(&s) } else { s });
        let payload_hex = event.payload_bytes.as_deref().map(|bytes| {
            if self.redact {
                // Only emit hex for raw payloads when explicitly unredacted; keep byte length in summary.
                String::new()
            } else {
                hex::encode(bytes)
            }
        });
        let payload_hex = payload_hex.filter(|s| !s.is_empty());

        let mut inner = self.inner.lock().expect("trace recorder lock poisoned");
        let seq = inner.seq;
        inner.seq = inner.seq.saturating_add(1);

        let record = TraceRecord {
            schema_version: SCHEMA_VERSION.to_owned(),
            seq,
            ts_ms,
            plane: event.plane,
            direction: event.direction,
            actor: self.actor,
            channel: event.channel,
            kind: event.kind,
            summary,
            payload_hex,
            payload_json,
            redacted: self.redact,
        };
        serde_json::to_writer(&mut inner.writer, &record).map_err(io::Error::other)?;
        inner.writer.write_all(b"\n")?;
        inner.writer.flush()
    }

    pub fn record_http_request(&self, method: &str, path: &str, body: &[u8]) {
        let mut summary = Map::new();
        summary.insert("method".into(), Value::String(method.to_owned()));
        summary.insert("path".into(), Value::String(path.to_owned()));
        summary.insert("body_len".into(), Value::from(body.len()));
        let payload_json = serde_json::from_slice::<Value>(body).ok();
        let _ = self.record(TraceEvent {
            plane: TracePlane::Control,
            direction: TraceDirection::Out,
            channel: None,
            kind: TraceKind::HttpRequest,
            summary: Some(summary),
            payload_bytes: None,
            payload_json,
        });
    }

    pub fn record_http_response(&self, method: &str, path: &str, status: u16, body: &[u8]) {
        let mut summary = Map::new();
        summary.insert("method".into(), Value::String(method.to_owned()));
        summary.insert("path".into(), Value::String(path.to_owned()));
        summary.insert("status".into(), Value::from(status));
        summary.insert("body_len".into(), Value::from(body.len()));
        let payload_json = serde_json::from_slice::<Value>(body).ok();
        let _ = self.record(TraceEvent {
            plane: TracePlane::Control,
            direction: TraceDirection::In,
            channel: None,
            kind: TraceKind::HttpResponse,
            summary: Some(summary),
            payload_bytes: None,
            payload_json,
        });
    }

    pub fn record_ws_text(&self, direction: TraceDirection, text: &str) {
        let mut summary = Map::new();
        summary.insert("len".into(), Value::from(text.len()));
        if let Some(prefix) = text.get(..text.find(['{', '[']).unwrap_or_else(|| text.len().min(4)))
        {
            summary.insert("prefix".into(), Value::String(prefix.to_owned()));
        }
        let payload_json = extract_socketio_json(text);
        let _ = self.record(TraceEvent {
            plane: TracePlane::Signal,
            direction,
            channel: None,
            kind: TraceKind::WsText,
            summary: Some(summary),
            payload_bytes: None,
            payload_json,
        });
    }

    pub fn record_ws_binary(&self, direction: TraceDirection, bytes: &[u8]) {
        let mut summary = Map::new();
        summary.insert("len".into(), Value::from(bytes.len()));
        let _ = self.record(TraceEvent {
            plane: TracePlane::Signal,
            direction,
            channel: None,
            kind: TraceKind::WsBinary,
            summary: Some(summary),
            payload_bytes: Some(bytes.to_vec()),
            payload_json: None,
        });
    }

    pub fn record_timeout(&self, plane: TracePlane, label: &str) {
        let mut summary = Map::new();
        summary.insert("label".into(), Value::String(label.to_owned()));
        let _ = self.record(TraceEvent {
            plane,
            direction: TraceDirection::In,
            channel: None,
            kind: TraceKind::Timeout,
            summary: Some(summary),
            payload_bytes: None,
            payload_json: None,
        });
    }
}

fn extract_socketio_json(text: &str) -> Option<Value> {
    let idx = text.find(['{', '['])?;
    serde_json::from_str(&text[idx..]).ok()
}

fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    matches!(
        lower.as_str(),
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
            | "iceservers"
    )
}

fn redact_map(map: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::with_capacity(map.len());
    for (k, v) in map {
        if is_secret_key(k) {
            out.insert(k.clone(), Value::String("<redacted>".into()));
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

fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_map(map)),
        Value::Array(arr) => Value::Array(arr.iter().map(redact_value).collect()),
        other => other.clone(),
    }
}

fn redact_sdp(sdp: &str) -> String {
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writes_jsonl_and_redacts_secrets() {
        let buf = SharedBuf::default();
        let recorder = TraceRecorder::new(TraceActor::Host, buf.clone());

        recorder.record_http_response(
            "POST",
            "/api/v1/login/by_qrcode",
            200,
            br#"{"code":0,"msg":"ok","data":{"token":"secret-jwt","user_id":"1"}}"#,
        );
        recorder.record_ws_text(
            TraceDirection::In,
            r#"42["soac",{"ice_id":"i1","sdp":"v=0\r\na=ice-ufrag:abc\r\na=ice-pwd:xyz\r\n"}]"#,
        );
        recorder.record_ws_binary(TraceDirection::Out, &[1, 2, 3, 4]);

        let raw = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(!raw.contains("secret-jwt"));
        assert!(!raw.contains("abc"));
        assert!(!raw.contains("xyz"));

        for (expected_seq, line) in lines.iter().enumerate() {
            let rec: TraceRecord = serde_json::from_str(line).unwrap();
            assert_eq!(rec.schema_version, "v0");
            assert_eq!(rec.seq, expected_seq as u64);
            assert_eq!(rec.actor, TraceActor::Host);
            assert!(rec.redacted);
        }

        let second: TraceRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second.kind, TraceKind::WsText);
        assert_eq!(second.payload_json.as_ref().unwrap()[0], json!("soac"));
    }
}
