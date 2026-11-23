//! Deterministic in-memory host responder for session tool channels (`file`, `terminal`, `tunnel`, `system`).

use std::collections::HashSet;

use remora_endpoint::role::{HostSession, HostToolEvent, HostToolSender, ToolEvent};
use remora_proto::pb::{
    self, draw_response, file_transfer_ftp_request, file_transfer_ftp_response, rpc_response,
    system_state_change,
};
use remora_proto::session::DataChannel;
use remora_proto::session::control::ControlMessage;
use remora_proto::session::file::{FileMessage, resolve_remote_path};
use remora_proto::session::system::{
    DrawAction, DrawOwnership, PrivacyAction, SystemMessage, normalize_privacy_request,
};
use remora_proto::session::terminal::{
    TERM_VERSION, TermFrame, TerminalControlMessage, resolve_close_target_session,
};
use remora_proto::session::tunnel::{TunnelFrame, TunnelFrameType, TunnelSynAck, decode_syn};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const DEFAULT_DOWNLOAD_DIR: &str = "/downloads";

/// Spawns a background task handling inbound tool-channel and system events for a [`HostSession`].
///
/// The task exits automatically when `session` closes and drops the event channel.
pub fn spawn_headless_tool_responder(session: &mut HostSession) -> Option<JoinHandle<()>> {
    let rx = session.take_tool_events()?;
    let sender = session.tool_sender();
    Some(tokio::spawn(run_responder(rx, sender)))
}

async fn run_responder(mut rx: mpsc::Receiver<HostToolEvent>, sender: HostToolSender) {
    let mut open_term_sessions: HashSet<u32> = HashSet::new();
    let mut draw_ownership = DrawOwnership::new();
    let mut next_seq: i64 = 1;

    while let Some(ev) = rx.recv().await {
        let ice_id = ev.ice_id;
        let platform = ev.controller_platform;
        match ev.event {
            ToolEvent::File(msg) => {
                handle_file(&sender, &ice_id, *msg).await;
            }
            ToolEvent::TerminalControl(msg) => {
                handle_terminal_control(&sender, &ice_id, msg, &mut next_seq).await;
            }
            ToolEvent::TermFrame(frame) => {
                handle_term_frame(&sender, &ice_id, frame, &mut open_term_sessions).await;
            }
            ToolEvent::Tunnel(frame) => {
                handle_tunnel(&sender, &ice_id, frame, &mut next_seq).await;
            }
            ToolEvent::System(msg) => {
                handle_system(
                    &sender,
                    &ice_id,
                    platform,
                    *msg,
                    &mut draw_ownership,
                    &mut next_seq,
                )
                .await;
            }
            _ => {}
        }
    }
}

async fn handle_file(sender: &HostToolSender, ice_id: &str, msg: FileMessage) {
    let FileMessage::Request {
        request_id,
        request,
        ..
    } = msg
    else {
        return;
    };
    let Some(which) = request.which else {
        return;
    };
    match which {
        file_transfer_ftp_request::Which::ReadDir(req) => {
            let (resolved, err, entries) =
                match resolve_remote_path(&req.path, DEFAULT_DOWNLOAD_DIR) {
                    Ok(path) => {
                        let base = path.trim_end_matches('/');
                        let full = format!("{base}/readme.txt");
                        (
                            path,
                            pb::FileTransferError::FtSucceeded as i32,
                            vec![pb::FileEntry {
                                entry_type: pb::FileType::FtFile as i32,
                                name: "readme.txt".to_owned(),
                                size: 12,
                                modified_time: 1_700_000_000,
                                full_path: full,
                                icon_type: "txt".to_owned(),
                            }],
                        )
                    }
                    Err(_) => (
                        String::new(),
                        pb::FileTransferError::FtProtocolError as i32,
                        Vec::new(),
                    ),
                };
            let _ = sender
                .send_file_response(
                    Some(ice_id),
                    request_id,
                    file_transfer_ftp_response::Which::FileDirectory(pb::FileDirectory {
                        id: req.id,
                        path: resolved,
                        entries,
                        file_error: err,
                        dir_data: Vec::new(),
                    }),
                )
                .await;
        }
        file_transfer_ftp_request::Which::FileBlock(req) => {
            let block_len = i32::try_from(req.data.len()).unwrap_or(i32::MAX);
            let _ = sender
                .send_file_response(
                    Some(ice_id),
                    request_id,
                    file_transfer_ftp_response::Which::BlockConfirm(pb::FileTransferBlockConfirm {
                        id: req.id,
                        block_id: req.block_id,
                        err: pb::FileTransferError::FtSucceeded as i32,
                        block_len,
                    }),
                )
                .await;
        }
        file_transfer_ftp_request::Which::RemoveDir(req) => {
            let _ = sender
                .send_file_response(
                    Some(ice_id),
                    request_id,
                    file_transfer_ftp_response::Which::Result(pb::FileTransferResult {
                        id: req.id,
                        file_error: pb::FileTransferError::FtSucceeded as i32,
                        err_msg: String::new(),
                    }),
                )
                .await;
        }
        file_transfer_ftp_request::Which::DirCreate(req) => {
            let _ = sender
                .send_file_response(
                    Some(ice_id),
                    request_id,
                    file_transfer_ftp_response::Which::OperationResult(pb::FileOperationResult {
                        file_error: pb::FileTransferError::FtSucceeded as i32,
                        err_msg: String::new(),
                        path: req.path,
                    }),
                )
                .await;
        }
        file_transfer_ftp_request::Which::RemoveFile(req) => {
            let _ = sender
                .send_file_response(
                    Some(ice_id),
                    request_id,
                    file_transfer_ftp_response::Which::OperationResult(pb::FileOperationResult {
                        file_error: pb::FileTransferError::FtSucceeded as i32,
                        err_msg: String::new(),
                        path: req.path,
                    }),
                )
                .await;
        }
        _ => {}
    }
}

async fn handle_terminal_control(
    sender: &HostToolSender,
    ice_id: &str,
    msg: TerminalControlMessage,
    next_seq: &mut i64,
) {
    if let TerminalControlMessage::CheckEnvRequest { request_id, .. } = msg {
        let seq = *next_seq;
        *next_seq += 1;
        let reply = ControlMessage::RpcResponse {
            seq,
            timestamp: 0,
            response: pb::RpcResponse {
                response_header: Some(pb::ResponseHeader { request_id }),
                which: Some(rpc_response::Which::TerminalCheckEnvResp(
                    pb::CheckEnvResp {
                        code: 0,
                        message: "ok".to_owned(),
                    },
                )),
            },
        };
        let _ = sender.send_control(Some(ice_id), reply).await;
    }
}

async fn handle_term_frame(
    sender: &HostToolSender,
    ice_id: &str,
    frame: TermFrame,
    open_sessions: &mut HashSet<u32>,
) {
    match frame.frame_type {
        // OPEN
        1 => {
            open_sessions.insert(frame.session_id);
            let payload = serde_json::to_vec(&json!({
                "ok": true,
                "session_id": frame.session_id,
            }))
            .unwrap_or_default();
            let _ = sender
                .send_term_frame(
                    Some(ice_id),
                    TermFrame {
                        version: TERM_VERSION,
                        frame_type: 1,
                        flags: [0, 0],
                        session_id: frame.session_id,
                        payload,
                    },
                )
                .await;
            let _ = sender.send_terminal_session_changed(Some(ice_id)).await;
        }
        // DATA
        2 => {
            let mut echoed = Vec::with_capacity(5 + frame.payload.len());
            echoed.extend_from_slice(b"echo:");
            echoed.extend_from_slice(&frame.payload);
            let _ = sender
                .send_term_frame(
                    Some(ice_id),
                    TermFrame {
                        version: TERM_VERSION,
                        frame_type: 2,
                        flags: [0, 0],
                        session_id: frame.session_id,
                        payload: echoed,
                    },
                )
                .await;
        }
        // CLOSE
        4 => {
            let target = resolve_close_target_session(frame.session_id, &frame.payload);
            open_sessions.remove(&target);
            let payload = serde_json::to_vec(&json!({
                "ok": true,
                "closed_session_id": target,
            }))
            .unwrap_or_default();
            let _ = sender
                .send_term_frame(
                    Some(ice_id),
                    TermFrame {
                        version: TERM_VERSION,
                        frame_type: 4,
                        flags: [0, 0],
                        session_id: frame.session_id,
                        payload,
                    },
                )
                .await;
        }
        _ => {}
    }
}

async fn handle_tunnel(
    sender: &HostToolSender,
    ice_id: &str,
    frame: TunnelFrame,
    next_seq: &mut i64,
) {
    let seq = *next_seq;
    *next_seq += 1;
    let reply = match frame.frame_type {
        TunnelFrameType::Syn => {
            let ack = match decode_syn(&frame.payload) {
                Ok(syn) => TunnelSynAck {
                    version: syn.version,
                    ok: true,
                    error: String::new(),
                },
                Err(_) => TunnelSynAck {
                    version: 1,
                    ok: false,
                    error: "invalid syn".to_owned(),
                },
            };
            TunnelFrame {
                seq,
                timestamp: 0,
                session_id: frame.session_id,
                rule_id: frame.rule_id,
                stream_id: frame.stream_id,
                frame_type: TunnelFrameType::SynAck,
                payload: serde_json::to_vec(&ack).unwrap_or_default(),
            }
        }
        TunnelFrameType::Data => TunnelFrame {
            seq,
            timestamp: 0,
            session_id: frame.session_id,
            rule_id: frame.rule_id,
            stream_id: frame.stream_id,
            frame_type: TunnelFrameType::Data,
            payload: frame.payload,
        },
        TunnelFrameType::Fin => TunnelFrame {
            seq,
            timestamp: 0,
            session_id: frame.session_id,
            rule_id: frame.rule_id,
            stream_id: frame.stream_id,
            frame_type: TunnelFrameType::Fin,
            payload: Vec::new(),
        },
        _ => return,
    };
    let _ = sender.send_tunnel_frame(Some(ice_id), reply).await;
}

async fn handle_system(
    sender: &HostToolSender,
    ice_id: &str,
    controller_platform: Option<u8>,
    msg: SystemMessage,
    draw_ownership: &mut DrawOwnership,
    next_seq: &mut i64,
) {
    match msg {
        SystemMessage::PrivacyRequest {
            request_id,
            request,
            ..
        } => {
            let cur_type = match normalize_privacy_request(&request, controller_platform) {
                PrivacyAction::Disable => pb::PrivateScreenType::PsNotSet,
                PrivacyAction::Enable(ty) => ty,
            };
            let _ = sender
                .send_privacy_response(
                    Some(ice_id),
                    request_id,
                    pb::PrivateScreenSettingResponse {
                        error_code: 0,
                        req_privacy_type: request.privacy_type,
                        cur_privacy_type: cur_type as i32,
                    },
                )
                .await;
        }
        SystemMessage::DrawRequest {
            request_id, action, ..
        } => {
            let accepted = draw_ownership.accept_or_take_over(ice_id, &action);
            let error_code = if accepted { 0 } else { -1 };
            let which = match action {
                DrawAction::Stroke { .. } => {
                    draw_response::Which::StrokeResponse(pb::DrawStrokeResponse { error_code })
                }
                DrawAction::Clear { .. } => {
                    draw_response::Which::ClearResponse(pb::DrawClearResponse { error_code })
                }
                DrawAction::Toggle { .. } => {
                    draw_response::Which::ToggleResponse(pb::DrawToggleResponse { error_code })
                }
                _ => return,
            };
            let _ = sender
                .send_draw_response(
                    Some(ice_id),
                    request_id,
                    pb::DrawResponse { which: Some(which) },
                )
                .await;
        }
        SystemMessage::VirtualAudioPolicyRequest { request_id, .. } => {
            let _ = sender
                .send_virtual_audio_policy_response(
                    Some(ice_id),
                    DataChannel::Control,
                    request_id,
                    0,
                )
                .await;
        }
        SystemMessage::QueryState { .. } => {
            let seq = *next_seq;
            *next_seq += 1;
            let _ = sender
                .send_control(
                    Some(ice_id),
                    ControlMessage::SystemStateChange {
                        seq,
                        timestamp: 0,
                        change: pb::SystemStateChange {
                            which: Some(system_state_change::Which::SdState(
                                pb::SecureDesktopState {
                                    is_locked: false,
                                    is_sas_shown: false,
                                    pos_x: 0,
                                    pos_y: 0,
                                    is_support_auto_logon: false,
                                    is_client_ui_ready: Some(true),
                                },
                            )),
                        },
                    },
                )
                .await;
        }
        _ => {}
    }
}
