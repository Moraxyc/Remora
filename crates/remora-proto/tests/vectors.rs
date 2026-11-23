use std::fs;
use std::path::{Path, PathBuf};

use remora_proto::control::{ApiError, SignKey, parse_response, sign};
use remora_proto::pb::{self, file_transfer_ftp_request, file_transfer_ftp_response};
use remora_proto::session::clipboard::{
    ClipboardMessage, encode_clipboard_change, encode_data_block, encode_format_data_ask,
    encode_format_list_request, encode_text_rpc_request, extract_clipboard_message,
};
use remora_proto::session::control::{ControlMessage, decode_message, encode_message};
use remora_proto::session::file::{
    FileMessage, encode_file_request, encode_file_response, encode_remove_dir_result,
    extract_file_message, resolve_remote_path,
};
use remora_proto::session::input::{InputMessage, MouseButton, decode_input};
use remora_proto::session::media::{
    MediaMessage, decode_capture_index_ext, decode_forward_setting, decode_new_frame_ext,
};
use remora_proto::session::system::{
    DrawAction, PrivacyAction, SystemMessage, encode_privacy_request, encode_privacy_response,
    encode_query_system_state, encode_virtual_audio_noop_probe_response, extract_system_message,
    normalize_privacy_request,
};
use remora_proto::session::terminal::{
    TermOpenOptions, TerminalControlMessage, decode_term_frame, encode_check_env_request,
    encode_rename_session_response, encode_session_changed, encode_term_frame,
    extract_terminal_control, resolve_close_target_session,
};
use remora_proto::session::tunnel::{
    TunnelFrameType, decode_syn, decode_syn_ack, encode_data, encode_fin, encode_syn,
    encode_syn_ack, encode_tunnel_frame, extract_tunnel_frame,
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

fn assert_rpc_envelope(seq: i64, timestamp: i64, request_id: i64, exp: &Value, name: &str) {
    assert_eq!(seq, exp["seq"].as_i64().unwrap(), "case {name} seq");
    assert_eq!(
        timestamp,
        exp["timestamp"].as_i64().unwrap(),
        "case {name} timestamp"
    );
    assert_eq!(
        request_id,
        exp["request_id"].as_i64().unwrap(),
        "case {name} request_id"
    );
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
                        "mouse_button" => {
                            assert_eq!(
                                msg,
                                InputMessage::MouseButton {
                                    button: i32::try_from(exp["button"].as_i64().unwrap()).unwrap(),
                                    pressed: exp["pressed"].as_bool().unwrap(),
                                },
                                "case {name}"
                            );
                            let expected_btn = match exp["decoded_button"].as_str().unwrap() {
                                "left" => MouseButton::Left,
                                "right" => MouseButton::Right,
                                "middle" => MouseButton::Middle,
                                "xbutton1" => MouseButton::XButton1,
                                "xbutton2" => MouseButton::XButton2,
                                other => panic!("unknown decoded_button {other}"),
                            };
                            assert_eq!(
                                msg.decoded_mouse_button(),
                                Some(expected_btn),
                                "case {name}"
                            );
                        }
                        "mouse_click" => {
                            assert_eq!(
                                msg,
                                InputMessage::MouseClick {
                                    button: i32::try_from(exp["button"].as_i64().unwrap()).unwrap(),
                                },
                                "case {name}"
                            );
                            let expected_btn = match exp["decoded_button"].as_str().unwrap() {
                                "left" => MouseButton::Left,
                                "right" => MouseButton::Right,
                                "middle" => MouseButton::Middle,
                                "xbutton1" => MouseButton::XButton1,
                                "xbutton2" => MouseButton::XButton2,
                                other => panic!("unknown decoded_button {other}"),
                            };
                            assert_eq!(
                                msg.decoded_mouse_button(),
                                Some(expected_btn),
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
            "session_clipboard" => {
                for case in cases {
                    let name = case["name"].as_str().unwrap();
                    let bytes = hex::decode(case["bytes_hex"].as_str().unwrap()).unwrap();
                    let ctrl = decode_message(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                    let msg = extract_clipboard_message(&ctrl)
                        .unwrap_or_else(|| panic!("case {name}: expected clipboard message"));
                    let exp = &case["expected"];
                    match exp["kind"].as_str().unwrap() {
                        "change" => {
                            let ClipboardMessage::Change {
                                seq,
                                timestamp,
                                content_id,
                                content,
                                to_be_continued,
                            } = msg
                            else {
                                panic!("case {name}: expected Change");
                            };
                            assert_eq!(seq, exp["seq"].as_i64().unwrap(), "case {name}");
                            assert_eq!(
                                timestamp,
                                exp["timestamp"].as_i64().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                content_id,
                                u32::try_from(exp["content_id"].as_u64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(content, exp["content"].as_str().unwrap(), "case {name}");
                            assert_eq!(
                                to_be_continued,
                                exp["to_be_continued"].as_bool().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_clipboard_change(
                                    seq,
                                    timestamp,
                                    content_id,
                                    &content,
                                    to_be_continued
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "text_rpc_request" => {
                            let ClipboardMessage::TextRpcRequest {
                                seq,
                                timestamp,
                                request_id,
                                format_id,
                                text,
                            } = msg
                            else {
                                panic!("case {name}: expected TextRpcRequest");
                            };
                            assert_eq!(seq, exp["seq"].as_i64().unwrap(), "case {name}");
                            assert_eq!(
                                timestamp,
                                exp["timestamp"].as_i64().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                request_id,
                                exp["request_id"].as_i64().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                format_id,
                                u32::try_from(exp["format_id"].as_u64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(text, exp["text"].as_str().unwrap(), "case {name}");
                            assert_eq!(
                                encode_text_rpc_request(seq, timestamp, request_id, &text),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "format_list_request" => {
                            let ClipboardMessage::FormatListRequest {
                                seq,
                                timestamp,
                                request_id,
                                formats,
                            } = msg
                            else {
                                panic!("case {name}: expected FormatListRequest");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            let got_ids: Vec<u64> =
                                formats.iter().map(|f| u64::from(f.id)).collect();
                            let exp_ids: Vec<u64> = exp["format_ids"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|v| v.as_u64().unwrap())
                                .collect();
                            assert_eq!(got_ids, exp_ids, "case {name}");
                            assert_eq!(
                                encode_format_list_request(seq, timestamp, request_id, formats),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "format_data_ask" => {
                            let ClipboardMessage::FormatDataAsk {
                                seq,
                                timestamp,
                                request_id,
                                format_id,
                                block_key,
                                format_name,
                            } = msg
                            else {
                                panic!("case {name}: expected FormatDataAsk");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            assert_eq!(
                                format_id,
                                u32::try_from(exp["format_id"].as_u64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                block_key,
                                exp["block_key"].as_str().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                format_name,
                                exp["format_name"].as_str().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_format_data_ask(
                                    seq,
                                    timestamp,
                                    request_id,
                                    format_id,
                                    &block_key,
                                    &format_name
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "data_block" => {
                            let ClipboardMessage::DataBlock {
                                seq,
                                timestamp,
                                request_id,
                                block_key,
                                block_id,
                                data,
                            } = msg
                            else {
                                panic!("case {name}: expected DataBlock");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            assert_eq!(
                                block_key,
                                exp["block_key"].as_str().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                block_id,
                                i32::try_from(exp["block_id"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                hex::encode(&data),
                                exp["data_hex"].as_str().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_data_block(
                                    seq, timestamp, request_id, &block_key, block_id, &data
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        other => panic!("unknown clipboard kind {other}"),
                    }
                }
            }
            "session_file" => {
                for case in cases {
                    let name = case["name"].as_str().unwrap();
                    let bytes = hex::decode(case["bytes_hex"].as_str().unwrap()).unwrap();
                    let ctrl = decode_message(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                    let msg = extract_file_message(&ctrl)
                        .unwrap_or_else(|| panic!("case {name}: expected file message"));
                    let exp = &case["expected"];
                    match exp["kind"].as_str().unwrap() {
                        "read_dir_request" => {
                            let FileMessage::Request {
                                seq,
                                timestamp,
                                request_id,
                                request,
                            } = msg
                            else {
                                panic!("case {name}: expected Request");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            let Some(file_transfer_ftp_request::Which::ReadDir(req)) =
                                request.which
                            else {
                                panic!("case {name}: expected ReadDir");
                            };
                            assert_eq!(
                                req.id.as_ref().unwrap().task_id,
                                i32::try_from(exp["task_id"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(req.path, exp["path"].as_str().unwrap(), "case {name}");
                            assert_eq!(
                                resolve_remote_path(&req.path, "/downloads").unwrap(),
                                exp["resolved_under_downloads"].as_str().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_file_request(
                                    seq,
                                    timestamp,
                                    request_id,
                                    file_transfer_ftp_request::Which::ReadDir(req)
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "file_block_request" => {
                            let FileMessage::Request {
                                seq,
                                timestamp,
                                request_id,
                                request,
                            } = msg
                            else {
                                panic!("case {name}: expected Request");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            let Some(file_transfer_ftp_request::Which::FileBlock(req)) =
                                request.which
                            else {
                                panic!("case {name}: expected FileBlock");
                            };
                            assert_eq!(
                                req.id.as_ref().unwrap().task_id,
                                i32::try_from(exp["task_id"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                req.block_id,
                                i32::try_from(exp["block_id"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                hex::encode(&req.data),
                                exp["data_hex"].as_str().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_file_request(
                                    seq,
                                    timestamp,
                                    request_id,
                                    file_transfer_ftp_request::Which::FileBlock(req)
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "remove_dir_result_response" => {
                            let FileMessage::Response {
                                seq,
                                timestamp,
                                request_id,
                                response,
                            } = msg
                            else {
                                panic!("case {name}: expected Response");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            let Some(file_transfer_ftp_response::Which::Result(res)) =
                                response.which
                            else {
                                panic!("case {name}: expected Result");
                            };
                            assert_eq!(
                                res.id.as_ref().unwrap().task_id,
                                i32::try_from(exp["task_id"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                res.file_error,
                                i32::try_from(exp["file_error"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_remove_dir_result(
                                    seq,
                                    timestamp,
                                    request_id,
                                    res.id.unwrap(),
                                    pb::FileTransferError::try_from(res.file_error).unwrap(),
                                    &res.err_msg,
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "block_confirm_response" => {
                            let FileMessage::Response {
                                seq,
                                timestamp,
                                request_id,
                                response,
                            } = msg
                            else {
                                panic!("case {name}: expected Response");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            let Some(file_transfer_ftp_response::Which::BlockConfirm(res)) =
                                response.which
                            else {
                                panic!("case {name}: expected BlockConfirm");
                            };
                            let id = res.id.as_ref().unwrap();
                            assert_eq!(
                                id.task_id,
                                i32::try_from(exp["task_id"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                id.file_index,
                                i32::try_from(exp["file_index"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                res.block_id,
                                i32::try_from(exp["block_id"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                res.block_len,
                                i32::try_from(exp["block_len"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                res.err,
                                i32::try_from(exp["err"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_file_response(
                                    seq,
                                    timestamp,
                                    request_id,
                                    file_transfer_ftp_response::Which::BlockConfirm(res),
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        other => panic!("unknown file kind {other}"),
                    }
                }
            }
            "session_terminal" => {
                for case in cases {
                    let name = case["name"].as_str().unwrap();
                    let bytes = hex::decode(case["bytes_hex"].as_str().unwrap()).unwrap();
                    let exp = &case["expected"];
                    match exp["kind"].as_str().unwrap() {
                        "term_frame" => {
                            let frame =
                                decode_term_frame(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                            assert_eq!(
                                frame.version,
                                u8::try_from(exp["version"].as_u64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                frame.frame_type,
                                u8::try_from(exp["frame_type"].as_u64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                frame.session_id,
                                u32::try_from(exp["session_id"].as_u64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            if let Some(cols) = exp.get("open_cols") {
                                let opts: TermOpenOptions =
                                    serde_json::from_slice(&frame.payload).unwrap();
                                assert_eq!(
                                    opts.cols,
                                    u16::try_from(cols.as_u64().unwrap()).unwrap(),
                                    "case {name}"
                                );
                                assert_eq!(
                                    opts.rows,
                                    u16::try_from(exp["open_rows"].as_u64().unwrap()).unwrap(),
                                    "case {name}"
                                );
                                assert_eq!(
                                    opts.shell,
                                    exp["open_shell"].as_str().unwrap(),
                                    "case {name}"
                                );
                            }
                            if let Some(target) = exp.get("close_target_session_id") {
                                assert_eq!(
                                    resolve_close_target_session(frame.session_id, &frame.payload),
                                    u32::try_from(target.as_u64().unwrap()).unwrap(),
                                    "case {name}"
                                );
                            }
                            assert_eq!(encode_term_frame(&frame), bytes, "case {name} re-encode");
                        }
                        "check_env_request" => {
                            let ctrl =
                                decode_message(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                            let Some(TerminalControlMessage::CheckEnvRequest {
                                seq,
                                timestamp,
                                request_id,
                            }) = extract_terminal_control(&ctrl)
                            else {
                                panic!("case {name}: expected CheckEnvRequest");
                            };
                            assert_eq!(seq, exp["seq"].as_i64().unwrap(), "case {name}");
                            assert_eq!(
                                timestamp,
                                exp["timestamp"].as_i64().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                request_id,
                                exp["request_id"].as_i64().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_check_env_request(seq, timestamp, request_id),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "rename_session_response" => {
                            let ctrl =
                                decode_message(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                            let Some(TerminalControlMessage::RenameSessionResponse {
                                seq,
                                timestamp,
                                request_id,
                                session_id,
                                code,
                                message,
                            }) = extract_terminal_control(&ctrl)
                            else {
                                panic!("case {name}: expected RenameSessionResponse");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            assert_eq!(
                                session_id,
                                u32::try_from(exp["session_id"].as_u64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                code,
                                i32::try_from(exp["code"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(message, exp["message"].as_str().unwrap(), "case {name}");
                            assert_eq!(
                                encode_rename_session_response(
                                    seq, timestamp, request_id, session_id, code, &message
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "session_changed" => {
                            let ctrl =
                                decode_message(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                            let Some(TerminalControlMessage::SessionChanged { seq, timestamp }) =
                                extract_terminal_control(&ctrl)
                            else {
                                panic!("case {name}: expected SessionChanged");
                            };
                            assert_eq!(seq, exp["seq"].as_i64().unwrap(), "case {name}");
                            assert_eq!(
                                timestamp,
                                exp["timestamp"].as_i64().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_session_changed(seq, timestamp),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        other => panic!("unknown terminal kind {other}"),
                    }
                }
            }
            "session_tunnel" => {
                for case in cases {
                    let name = case["name"].as_str().unwrap();
                    let bytes = hex::decode(case["bytes_hex"].as_str().unwrap()).unwrap();
                    let ctrl = decode_message(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                    let frame = extract_tunnel_frame(&ctrl)
                        .unwrap_or_else(|| panic!("case {name}: expected tunnel frame"));
                    let exp = &case["expected"];
                    assert_eq!(frame.seq, exp["seq"].as_i64().unwrap(), "case {name}");
                    assert_eq!(
                        frame.timestamp,
                        exp["timestamp"].as_i64().unwrap(),
                        "case {name}"
                    );
                    assert_eq!(
                        frame.session_id,
                        exp["session_id"].as_u64().unwrap(),
                        "case {name}"
                    );
                    assert_eq!(
                        frame.rule_id,
                        exp["rule_id"].as_u64().unwrap(),
                        "case {name}"
                    );
                    assert_eq!(
                        frame.stream_id,
                        exp["stream_id"].as_u64().unwrap(),
                        "case {name}"
                    );
                    match exp["frame_type"].as_str().unwrap() {
                        "syn" => {
                            assert_eq!(frame.frame_type, TunnelFrameType::Syn, "case {name}");
                            let syn = decode_syn(&frame.payload).unwrap();
                            assert_eq!(
                                syn.target_host,
                                exp["syn_host"].as_str().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                syn.target_port,
                                u16::try_from(exp["syn_port"].as_u64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_syn(
                                    frame.seq,
                                    frame.timestamp,
                                    frame.session_id,
                                    frame.rule_id,
                                    frame.stream_id,
                                    &syn
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "syn_ack" => {
                            assert_eq!(frame.frame_type, TunnelFrameType::SynAck, "case {name}");
                            let ack = decode_syn_ack(&frame.payload).unwrap();
                            assert_eq!(ack.ok, exp["syn_ack_ok"].as_bool().unwrap(), "case {name}");
                            assert_eq!(
                                encode_syn_ack(
                                    frame.seq,
                                    frame.timestamp,
                                    frame.session_id,
                                    frame.rule_id,
                                    frame.stream_id,
                                    &ack
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "data" => {
                            assert_eq!(frame.frame_type, TunnelFrameType::Data, "case {name}");
                            assert_eq!(
                                hex::encode(&frame.payload),
                                exp["payload_hex"].as_str().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_data(
                                    frame.seq,
                                    frame.timestamp,
                                    frame.session_id,
                                    frame.rule_id,
                                    frame.stream_id,
                                    frame.payload.clone()
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "fin" => {
                            assert_eq!(frame.frame_type, TunnelFrameType::Fin, "case {name}");
                            assert!(frame.payload.is_empty(), "case {name}");
                            assert_eq!(
                                encode_fin(
                                    frame.seq,
                                    frame.timestamp,
                                    frame.session_id,
                                    frame.rule_id,
                                    frame.stream_id
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "unknown" => {
                            let code =
                                i32::try_from(exp["unknown_code"].as_i64().unwrap()).unwrap();
                            assert_eq!(
                                frame.frame_type,
                                TunnelFrameType::Unknown(code),
                                "case {name}"
                            );
                            assert_eq!(
                                hex::encode(&frame.payload),
                                exp["payload_hex"].as_str().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(encode_tunnel_frame(&frame), bytes, "case {name} re-encode");
                        }
                        other => panic!("unknown tunnel frame_type {other}"),
                    }
                }
            }
            "session_system" => {
                for case in cases {
                    let name = case["name"].as_str().unwrap();
                    let bytes = hex::decode(case["bytes_hex"].as_str().unwrap()).unwrap();
                    let ctrl = decode_message(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                    let msg = extract_system_message(&ctrl)
                        .unwrap_or_else(|| panic!("case {name}: expected system message"));
                    let exp = &case["expected"];
                    match exp["kind"].as_str().unwrap() {
                        "privacy_request" => {
                            let SystemMessage::PrivacyRequest {
                                seq,
                                timestamp,
                                request_id,
                                request,
                            } = msg
                            else {
                                panic!("case {name}: expected PrivacyRequest");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            assert_eq!(
                                request.enable,
                                exp["enable"].as_bool().unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                request.privacy_type,
                                i32::try_from(exp["privacy_type"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            let expected_norm = match exp["windows_normalized"].as_str().unwrap() {
                                "disable" => PrivacyAction::Disable,
                                "enable_normal" => {
                                    PrivacyAction::Enable(pb::PrivateScreenType::PsNormalPrivacy)
                                }
                                "enable_enhanced" => {
                                    PrivacyAction::Enable(pb::PrivateScreenType::PsEnhancedPrivacy)
                                }
                                other => panic!("unknown windows_normalized {other}"),
                            };
                            assert_eq!(
                                normalize_privacy_request(&request, Some(1)),
                                expected_norm,
                                "case {name}"
                            );
                            assert_eq!(
                                encode_privacy_request(seq, timestamp, request_id, request),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "privacy_response" => {
                            let SystemMessage::PrivacyResponse {
                                seq,
                                timestamp,
                                request_id,
                                response,
                            } = msg
                            else {
                                panic!("case {name}: expected PrivacyResponse");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            assert_eq!(
                                response.error_code,
                                i32::try_from(exp["error_code"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                response.req_privacy_type,
                                i32::try_from(exp["req_privacy_type"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                response.cur_privacy_type,
                                i32::try_from(exp["cur_privacy_type"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_privacy_response(seq, timestamp, request_id, response),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "draw_toggle_request" => {
                            let SystemMessage::DrawRequest {
                                seq,
                                timestamp,
                                request_id,
                                action,
                            } = msg
                            else {
                                panic!("case {name}: expected DrawRequest");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            assert_eq!(
                                action,
                                DrawAction::Toggle {
                                    enable: exp["enable"].as_bool().unwrap()
                                },
                                "case {name}"
                            );
                            assert_eq!(encode_message(&ctrl), bytes, "case {name} re-encode");
                        }
                        "virtual_audio_policy_response" => {
                            let SystemMessage::VirtualAudioPolicyResponse {
                                seq,
                                timestamp,
                                request_id,
                                error_code,
                            } = msg
                            else {
                                panic!("case {name}: expected VirtualAudioPolicyResponse");
                            };
                            assert_rpc_envelope(seq, timestamp, request_id, exp, name);
                            assert_eq!(
                                error_code,
                                i32::try_from(exp["error_code"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_virtual_audio_noop_probe_response(
                                    seq, timestamp, request_id
                                ),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        "query_system_state" => {
                            let SystemMessage::QueryState { seq, timestamp, id } = msg else {
                                panic!("case {name}: expected QueryState");
                            };
                            assert_eq!(seq, exp["seq"].as_i64().unwrap(), "case {name} seq");
                            assert_eq!(
                                timestamp,
                                exp["timestamp"].as_i64().unwrap(),
                                "case {name} timestamp"
                            );
                            assert_eq!(
                                id,
                                i32::try_from(exp["id"].as_i64().unwrap()).unwrap(),
                                "case {name}"
                            );
                            assert_eq!(
                                encode_query_system_state(seq, timestamp, id),
                                bytes,
                                "case {name} re-encode"
                            );
                        }
                        other => panic!("unknown system kind {other}"),
                    }
                }
            }
            other => panic!("unknown session suite {other}"),
        }
    }
}
