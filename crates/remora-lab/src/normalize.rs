//! Symbolizes volatile runtime identifiers while preserving equality relationships across a run.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::scenario::ObservedStep;
use crate::scenario::runner::ScenarioResult;

/// Canonical, target-independent representation of a scenario execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedRun {
    pub scenario: String,
    pub passed: bool,
    pub error: Option<String>,
    pub steps: Vec<NormalizedStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedStep {
    pub index: usize,
    pub actor: String,
    pub action: String,
    pub http_status: Option<u16>,
    pub code: Option<i64>,
    pub ack_status: Option<String>,
    pub event_name: Option<String>,
    pub payload: Value,
}

#[derive(Default)]
struct SymbolTable {
    counters: HashMap<&'static str, usize>,
    seen: HashMap<(&'static str, String), String>,
}

impl SymbolTable {
    fn intern(&mut self, category: &'static str, raw: &str) -> String {
        let key = (category, raw.to_owned());
        if let Some(existing) = self.seen.get(&key) {
            return existing.clone();
        }
        let idx = self.counters.entry(category).or_insert(0);
        let symbol = format!("${category}_{}", *idx);
        *idx += 1;
        self.seen.insert(key, symbol.clone());
        symbol
    }

    fn normalize_url_origin(&mut self, raw: &str) -> String {
        if let Ok(mut parsed) = url::Url::parse(raw) {
            let scheme = parsed.scheme().to_owned();
            let path = parsed.path().to_owned();
            if path.starts_with("/login/") {
                let sym = self.intern("jump_url", raw);
                return sym;
            }
            let _ = parsed.set_host(Some("remora.local"));
            let _ = parsed.set_port(None);
            if parsed.path() == "/" {
                return format!("{scheme}://remora.local");
            }
            return parsed.to_string();
        }
        raw.to_owned()
    }

    fn normalize_value(&mut self, key_hint: Option<&str>, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.normalize_string(key_hint, s)),
            Value::Array(arr) => {
                let mut items: Vec<Value> = arr
                    .iter()
                    .map(|item| self.normalize_value(key_hint, item))
                    .collect();
                if matches!(
                    key_hint,
                    Some("my_binded_devices" | "desktop_devices" | "mobile_devices" | "tv_devices")
                ) {
                    items.sort_by_cached_key(|v| v.to_string());
                }
                Value::Array(items)
            }
            Value::Object(map) => {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (k, v) in map {
                    if matches!(
                        k.as_str(),
                        "controlled_time" | "last_connected_at" | "favorited_at"
                    ) && v.is_u64()
                    {
                        out.insert(k.clone(), Value::from(0));
                    } else {
                        out.insert(k.clone(), self.normalize_value(Some(k.as_str()), v));
                    }
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }

    fn normalize_string(&mut self, key_hint: Option<&str>, raw: &str) -> String {
        if raw.is_empty() {
            return String::new();
        }
        match key_hint {
            Some("device_id" | "current_device_id" | "publisher_device_id" | "room_id") => {
                self.intern("device", raw)
            }
            Some("connect_id" | "share_id") => self.intern("connect_id", raw),
            Some("guest_id") => self.intern("guest", raw),
            Some("control_id") => self.intern("control", raw),
            Some("salt") => self.intern("salt", raw),
            Some("client_id") => self.intern("client", raw),
            Some("ice_id") => self.intern("ice", raw),
            Some("token") => {
                if raw.contains('.') {
                    self.intern("jwt", raw)
                } else {
                    self.intern("token", raw)
                }
            }
            Some("status_query_ticket") => self.intern("ticket", raw),
            Some("qrcode_jump_url") => self.intern("jump_url", raw),
            Some("reconnect_key") => self.intern("reconnect_key", raw),
            Some("username") => self.intern("turn_user", raw),
            Some("credential") => self.intern("turn_cred", raw),
            Some("urls") => self.intern("ice_url", raw),
            Some(
                "signaling_server" | "signaling_list" | "sni_required_signaling_list" | "req_url",
            ) => self.normalize_url_origin(raw),
            Some("url" | "wallpaper_url") if raw.contains("/api/files/") => {
                self.intern("file_url", raw)
            }
            Some("sdp") => redact_and_symbolize_sdp(raw),
            _ => raw.to_owned(),
        }
    }
}

fn redact_and_symbolize_sdp(sdp: &str) -> String {
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        if line.starts_with("a=ice-ufrag:") {
            out.push_str("a=ice-ufrag:$ufrag\r\n");
        } else if line.starts_with("a=ice-pwd:") {
            out.push_str("a=ice-pwd:$pwd\r\n");
        } else if line.starts_with("a=fingerprint:") {
            out.push_str("a=fingerprint:$fp\r\n");
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Normalizes a [`ScenarioResult`] into a [`NormalizedRun`].
pub fn normalize_run(result: &ScenarioResult) -> NormalizedRun {
    let mut table = SymbolTable::default();
    let steps = result
        .steps
        .iter()
        .map(|step| normalize_step(&mut table, step))
        .collect();
    NormalizedRun {
        scenario: result.scenario.clone(),
        passed: result.passed,
        error: result.error.clone(),
        steps,
    }
}

fn normalize_step(table: &mut SymbolTable, step: &ObservedStep) -> NormalizedStep {
    NormalizedStep {
        index: step.index,
        actor: step.actor.clone(),
        action: step.action.clone(),
        http_status: step.http_status,
        code: step.code,
        ack_status: step.ack_status.clone(),
        event_name: step.event_name.clone(),
        payload: table.normalize_value(None, &step.payload),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample_result(port: u16, device_id: &str, ice_id: &str) -> ScenarioResult {
        ScenarioResult {
            scenario: "connect".into(),
            target: "test".into(),
            passed: true,
            error: None,
            trace: Vec::new(),
            duration_ms: port as u64,
            steps: vec![
                ObservedStep {
                    index: 0,
                    id: None,
                    actor: "host".into(),
                    action: "device_init".into(),
                    http_status: Some(200),
                    code: Some(0),
                    ack_status: None,
                    event_name: None,
                    payload: json!({ "device_id": device_id }),
                },
                ObservedStep {
                    index: 1,
                    id: None,
                    actor: "host".into(),
                    action: "room_create".into(),
                    http_status: Some(200),
                    code: Some(0),
                    ack_status: None,
                    event_name: None,
                    payload: json!({
                        "signaling_server": format!("ws://127.0.0.1:{port}"),
                        "signaling_list": [format!("ws://127.0.0.1:{port}")],
                        "token": "hdr.pay.sig"
                    }),
                },
                ObservedStep {
                    index: 2,
                    id: None,
                    actor: "host".into(),
                    action: "room_info".into(),
                    http_status: None,
                    code: None,
                    ack_status: Some("ok".into()),
                    event_name: None,
                    payload: json!([{ "room_id": device_id, "publisher": { "device_id": device_id } }]),
                },
                ObservedStep {
                    index: 3,
                    id: None,
                    actor: "viewer".into(),
                    action: "send_control".into(),
                    http_status: None,
                    code: None,
                    ack_status: Some("success".into()),
                    event_name: Some("control_ack".into()),
                    payload: json!({ "ice_id": ice_id, "client_id": "c-host" }),
                },
                ObservedStep {
                    index: 4,
                    id: None,
                    actor: "host".into(),
                    action: "wait_event".into(),
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("be-controlled".into()),
                    payload: json!({ "ice_id": ice_id, "client_id": "c-viewer" }),
                },
            ],
        }
    }

    #[test]
    fn equal_shapes_with_different_volatile_ids_normalize_identically() {
        let a = normalize_run(&sample_result(40001, "uuid-aaa", "ice-111"));
        let b = normalize_run(&sample_result(50002, "uuid-bbb", "ice-222"));
        assert_eq!(a, b);
        assert_eq!(a.steps[2].payload[0]["room_id"], "$device_0");
        assert_eq!(a.steps[3].payload["ice_id"], "$ice_0");
        assert_eq!(a.steps[4].payload["ice_id"], "$ice_0");
    }

    #[test]
    fn inequality_in_id_linking_is_preserved() {
        let mut r = sample_result(40001, "uuid-aaa", "ice-111");
        r.steps[4].payload["ice_id"] = json!("ice-DIFFERENT");
        let n = normalize_run(&r);
        assert_ne!(n.steps[3].payload["ice_id"], n.steps[4].payload["ice_id"]);
    }
}
