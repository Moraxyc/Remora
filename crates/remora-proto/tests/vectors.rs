use std::fs;
use std::path::{Path, PathBuf};

use remora_proto::control::{ApiError, SignKey, parse_response, sign};
use remora_proto::session::control::{ControlMessage, decode_message};
use remora_proto::session::input::{InputMessage, decode_input};
use remora_proto::session::media::{
    MediaMessage, decode_capture_index_ext, decode_forward_setting, decode_new_frame_ext,
};
use remora_proto::signal::{Decoder, Packet, SignalEvent};
use serde_json::Value;

fn compat_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat")
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn read_json(path: &Path) -> Value {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

#[test]
fn schemas_are_valid_v0_json() {
    let schema_dir = compat_dir().join("schema");
    for name in ["meta.schema.json", "trace.schema.json"] {
        let doc = read_json(&schema_dir.join(name));
        assert_eq!(doc["properties"]["schema_version"]["const"], "v0");
    }
}

#[test]
fn runs_control_vectors() {
    let dir = compat_dir().join("vectors/control");
    let files = json_files(&dir);
    assert!(!files.is_empty());

    for path in files {
        let doc = read_json(&path);
        let suite = doc["suite"].as_str().unwrap();
        let cases = doc["cases"].as_array().unwrap();
        match suite {
            "control_signing" => {
                for case in cases {
                    let name = case["name"].as_str().unwrap();
                    let key = SignKey::new(case["key"].as_str().unwrap());
                    let method = case["method"].as_str().unwrap();
                    let path_q = case["path"].as_str().unwrap();
                    let headers: Vec<(&str, &str)> = case["headers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|pair| (pair[0].as_str().unwrap(), pair[1].as_str().unwrap()))
                        .collect();
                    let body = hex::decode(case["body_hex"].as_str().unwrap()).unwrap();
                    let expected = case["expected_signature"].as_str().unwrap();
                    assert_eq!(
                        sign(&key, method, path_q, headers, &body),
                        expected,
                        "case {name}"
                    );
                }
            }
            "control_responses" => {
                for case in cases {
                    let name = case["name"].as_str().unwrap();
                    let status = u16::try_from(case["http_status"].as_u64().unwrap()).unwrap();
                    let body = hex::decode(case["body_hex"].as_str().unwrap()).unwrap();
                    let expected = &case["expected"];
                    let res = parse_response::<Value>(status, &body);
                    match expected["outcome"].as_str().unwrap() {
                        "ok" => assert_eq!(res.unwrap(), expected["data"], "case {name}"),
                        "token_expired" => {
                            assert_eq!(res.unwrap_err(), ApiError::TokenExpired, "case {name}")
                        }
                        "qr_code_expired" => {
                            assert_eq!(res.unwrap_err(), ApiError::QrCodeExpired, "case {name}")
                        }
                        "rejected" => assert_eq!(
                            res.unwrap_err(),
                            ApiError::Rejected {
                                http_status: u16::try_from(
                                    expected["http_status"].as_u64().unwrap()
                                )
                                .unwrap(),
                                code: expected["code"].as_i64().unwrap(),
                            },
                            "case {name}"
                        ),
                        "malformed" => assert_eq!(
                            res.unwrap_err(),
                            ApiError::Malformed {
                                http_status: u16::try_from(
                                    expected["http_status"].as_u64().unwrap()
                                )
                                .unwrap(),
                            },
                            "case {name}"
                        ),
                        other => panic!("unknown outcome {other} in {name}"),
                    }
                }
            }
            other => panic!("unknown control suite {other}"),
        }
    }
}

#[test]
fn runs_signal_vectors() {
    let dir = compat_dir().join("vectors/signal");
    let files = json_files(&dir);
    assert!(!files.is_empty());

    for path in files {
        let doc = read_json(&path);
        let cases = doc["cases"].as_array().unwrap();
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut decoder = Decoder::new();
            let mut decoded = None;
            for frame in case["frames"].as_array().unwrap() {
                let bytes = hex::decode(frame["hex"].as_str().unwrap()).unwrap();
                decoded = match frame["kind"].as_str().unwrap() {
                    "text" => decoder
                        .text(std::str::from_utf8(&bytes).unwrap())
                        .unwrap_or_else(|e| panic!("case {name}: {e}")),
                    "binary" => decoder
                        .binary(&bytes)
                        .unwrap_or_else(|e| panic!("case {name}: {e}")),
                    other => panic!("unknown frame kind {other}"),
                };
            }
            let packet = decoded.unwrap_or_else(|| panic!("case {name} incomplete"));
            let expected = &case["expected"];
            match expected["packet"].as_str().unwrap() {
                "open" => {
                    let Packet::Open(hs) = packet else {
                        panic!("case {name}: expected open");
                    };
                    assert_eq!(hs.sid, expected["sid"].as_str().unwrap(), "case {name}");
                    assert_eq!(
                        hs.ping_interval,
                        expected["ping_interval"].as_u64().unwrap(),
                        "case {name}"
                    );
                    assert_eq!(
                        hs.ping_timeout,
                        expected["ping_timeout"].as_u64().unwrap(),
                        "case {name}"
                    );
                }
                "ping" => assert_eq!(packet, Packet::Ping, "case {name}"),
                "pong" => assert_eq!(packet, Packet::Pong, "case {name}"),
                "connected" => assert_eq!(packet, Packet::Connected, "case {name}"),
                "event" => {
                    let Packet::Event(ev) = packet else {
                        panic!("case {name}: expected event");
                    };
                    assert_eq!(
                        ev.name,
                        expected["event_name"].as_str().unwrap(),
                        "case {name}"
                    );
                    if let Some(arr) = expected.get("attachments_hex").and_then(Value::as_array) {
                        let actual: Vec<String> = ev.attachments.iter().map(hex::encode).collect();
                        let exp: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
                        assert_eq!(actual, exp, "case {name}");
                    }
                    if expected.get("signal_variant").and_then(Value::as_str) == Some("unknown") {
                        assert!(
                            matches!(SignalEvent::from_event(&ev), SignalEvent::Unknown { .. }),
                            "case {name}"
                        );
                    }
                }
                "ack" => {
                    let Packet::Ack(ack) = packet else {
                        panic!("case {name}: expected ack");
                    };
                    assert_eq!(ack.id, expected["id"].as_u64().unwrap(), "case {name}");
                }
                other => panic!("unknown expected packet {other}"),
            }
        }
    }
}

#[test]
fn runs_session_vectors() {
    let dir = compat_dir().join("vectors/session");
    let files = json_files(&dir);
    assert!(!files.is_empty());

    for path in files {
        let doc = read_json(&path);
        let suite = doc["suite"].as_str().unwrap();
        let cases = doc["cases"].as_array().unwrap();
        match suite {
            "session_control" => {
                for case in cases {
                    let name = case["name"].as_str().unwrap();
                    let bytes = hex::decode(case["bytes_hex"].as_str().unwrap()).unwrap();
                    let msg = decode_message(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                    let exp = &case["expected"];
                    match exp["kind"].as_str().unwrap() {
                        "echo_response" => {
                            let ControlMessage::EchoResponse {
                                seq,
                                timestamp,
                                request_seq,
                                feature_flag,
                                ..
                            } = msg
                            else {
                                panic!("case {name}: expected EchoResponse");
                            };
                            assert_eq!(seq, exp["seq"].as_i64().unwrap(), "case {name}");
                            assert_eq!(
                                timestamp,
                                exp["timestamp"].as_i64().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                request_seq,
                                Some(exp["request_seq"].as_i64().unwrap()),
                                "case {name}"
                            );
                            assert_eq!(
                                feature_flag.unwrap().ff_capture_setting,
                                i32::try_from(exp["ff_capture_setting"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                        }
                        "simple_action" => {
                            let ControlMessage::SimpleAction { action, .. } = msg else {
                                panic!("case {name}: expected SimpleAction");
                            };
                            assert_eq!(
                                action.action,
                                i32::try_from(exp["action"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                        }
                        "unknown" => {
                            assert!(matches!(msg, ControlMessage::Unknown { .. }), "case {name}");
                        }
                        other => panic!("unknown expected kind {other}"),
                    }
                }
            }
            "session_input" => {
                for case in cases {
                    let name = case["name"].as_str().unwrap();
                    let bytes = hex::decode(case["bytes_hex"].as_str().unwrap()).unwrap();
                    let msg = decode_input(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                    let exp = &case["expected"];
                    match exp["kind"].as_str().unwrap() {
                        "keyboard_heartbeat" => {
                            assert_eq!(msg, InputMessage::KeyboardHeartbeat, "case {name}");
                        }
                        "keyboard" => {
                            assert_eq!(
                                msg,
                                InputMessage::Keyboard {
                                    key: i32::try_from(exp["key"].as_i64().unwrap()).unwrap(),
                                    pressed: exp["pressed"].as_bool().unwrap(),
                                },
                                "case {name}"
                            );
                        }
                        "mouse_absolute" => {
                            assert_eq!(
                                msg,
                                InputMessage::MouseAbsolute {
                                    x: exp["x"].as_f64().unwrap(),
                                    y: exp["y"].as_f64().unwrap(),
                                },
                                "case {name}"
                            );
                        }
                        "unknown" => {
                            assert!(
                                matches!(msg, InputMessage::Unknown { ref name, .. } if name == exp["name"].as_str().unwrap()),
                                "case {name}"
                            );
                        }
                        other => panic!("unknown input kind {other}"),
                    }
                }
            }
            "session_media" => {
                for case in cases {
                    let name = case["name"].as_str().unwrap();
                    let bytes = hex::decode(case["bytes_hex"].as_str().unwrap()).unwrap();
                    match case["kind"].as_str().unwrap() {
                        "rtp_capture_index" => {
                            assert_eq!(
                                decode_capture_index_ext(&bytes),
                                Some(
                                    u16::try_from(case["expected_index"].as_u64().unwrap())
                                        .unwrap()
                                ),
                                "case {name}"
                            );
                        }
                        "rtp_new_frame" => {
                            assert_eq!(
                                decode_new_frame_ext(&bytes),
                                Some(case["expected_new_frame"].as_bool().unwrap()),
                                "case {name}"
                            );
                        }
                        "forward_setting" => {
                            let val: Value = serde_json::from_slice(&bytes).unwrap();
                            let decoded = decode_forward_setting(&val);
                            match case["expected_variant"].as_str().unwrap() {
                                "device_capability" => assert!(
                                    matches!(decoded, MediaMessage::DeviceCapability(_)),
                                    "case {name}"
                                ),
                                other => panic!("unknown media variant {other}"),
                            }
                        }
                        other => panic!("unknown media kind {other}"),
                    }
                }
            }
            other => panic!("unknown session suite {other}"),
        }
    }
}
