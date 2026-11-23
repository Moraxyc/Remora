//! Privacy screen, annotation, virtual audio policy, and system state messages.

use std::fmt;

use super::{
    DataChannel,
    control::{ControlMessage, encode_message},
};
use crate::pb::{
    self, draw_request, draw_response, rpc_request, rpc_response, system_state_change,
};

pub const PRIVACY_REPLY_CHANNEL: DataChannel = DataChannel::Text;
/// The macOS viewer times out (9998) if draw replies arrive on the control channel.
pub const DRAW_REPLY_CHANNEL: DataChannel = DataChannel::Text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyAction {
    Disable,
    Enable(pb::PrivateScreenType),
}

pub fn normalize_privacy_request(
    req: &pb::PrivateScreenSettingRequest,
    controller_platform: Option<u8>,
) -> PrivacyAction {
    use pb::PrivateScreenType::{PsEnhancedPrivacy, PsNormalPrivacy};
    match (req.enable, controller_platform, req.privacy_type()) {
        (true, _, PsEnhancedPrivacy) => PrivacyAction::Enable(PsEnhancedPrivacy),
        (true, _, _) => PrivacyAction::Enable(PsNormalPrivacy),
        (false, Some(1), ty @ (PsNormalPrivacy | PsEnhancedPrivacy)) => PrivacyAction::Enable(ty),
        (false, Some(3), PsEnhancedPrivacy) => PrivacyAction::Enable(PsEnhancedPrivacy),
        _ => PrivacyAction::Disable,
    }
}

#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum DrawAction {
    Stroke {
        stroke_id: u32,
        points: Vec<pb::DrawPoint>,
        screen_id: Option<i32>,
        line_width: Option<f32>,
        color: Option<u32>,
    },
    Clear {
        clear_type: i32,
        stroke_id: u32,
        screen_id: Option<i32>,
    },
    Toggle {
        enable: bool,
    },
}

impl fmt::Debug for DrawAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stroke {
                stroke_id,
                points,
                screen_id,
                line_width,
                color,
            } => f
                .debug_struct("Stroke")
                .field("stroke_id", stroke_id)
                .field("points_len", &points.len())
                .field("screen_id", screen_id)
                .field("has_line_width", &line_width.is_some())
                .field("has_color", &color.is_some())
                .finish(),
            Self::Clear {
                clear_type,
                stroke_id,
                screen_id,
            } => f
                .debug_struct("Clear")
                .field("clear_type", clear_type)
                .field("stroke_id", stroke_id)
                .field("screen_id", screen_id)
                .finish(),
            Self::Toggle { enable } => f.debug_struct("Toggle").field("enable", enable).finish(),
        }
    }
}

impl From<&draw_request::Which> for DrawAction {
    fn from(req: &draw_request::Which) -> Self {
        match req {
            draw_request::Which::StrokeRequest(s) => Self::Stroke {
                stroke_id: s.stroke_id,
                points: s.points.clone(),
                screen_id: s.screen_id,
                line_width: s.line_width,
                color: s.color,
            },
            draw_request::Which::ClearRequest(c) => Self::Clear {
                clear_type: c.clear_type,
                stroke_id: c.stroke_id,
                screen_id: c.screen_id,
            },
            draw_request::Which::ToggleRequest(t) => Self::Toggle { enable: t.enable },
        }
    }
}

#[derive(Default)]
pub struct DrawOwnership {
    active_owner: Option<String>,
}

impl fmt::Debug for DrawOwnership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DrawOwnership")
            .field("has_owner", &self.active_owner.is_some())
            .finish()
    }
}

impl DrawOwnership {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_over(&mut self, peer_id: &str) {
        if self.active_owner.as_deref() != Some(peer_id) {
            self.active_owner = Some(peer_id.to_owned());
        }
    }

    pub fn accept_or_take_over(&mut self, peer_id: &str, action: &DrawAction) -> bool {
        match action {
            DrawAction::Toggle { enable: true } | DrawAction::Stroke { .. } => {
                self.take_over(peer_id);
                true
            }
            DrawAction::Toggle { enable: false } => self.reset_peer(peer_id),
            DrawAction::Clear { .. } => self.active_owner.as_deref() == Some(peer_id),
        }
    }

    pub fn reset_peer(&mut self, peer_id: &str) -> bool {
        if self.active_owner.as_deref() != Some(peer_id) {
            return false;
        }
        self.active_owner = None;
        true
    }
}

#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum SystemMessage {
    PrivacyRequest {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        request: pb::PrivateScreenSettingRequest,
    },
    PrivacyResponse {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        response: pb::PrivateScreenSettingResponse,
    },
    DrawRequest {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        action: DrawAction,
    },
    DrawResponse {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        response: pb::DrawResponse,
    },
    VirtualAudioPolicyRequest {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        policy: i32,
    },
    VirtualAudioPolicyResponse {
        seq: i64,
        timestamp: i64,
        request_id: i64,
        error_code: i32,
    },
    StateChange {
        seq: i64,
        timestamp: i64,
        change: pb::SystemStateChange,
    },
    QueryState {
        seq: i64,
        timestamp: i64,
        id: i32,
    },
}

impl fmt::Debug for SystemMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrivacyRequest {
                request_id,
                request,
                ..
            } => f
                .debug_struct("PrivacyRequest")
                .field("request_id", request_id)
                .field("enable", &request.enable)
                .field("privacy_type", &request.privacy_type)
                .finish(),
            Self::PrivacyResponse {
                request_id,
                response,
                ..
            } => f
                .debug_struct("PrivacyResponse")
                .field("request_id", request_id)
                .field("error_code", &response.error_code)
                .finish(),
            Self::DrawRequest {
                request_id, action, ..
            } => f
                .debug_struct("DrawRequest")
                .field("request_id", request_id)
                .field("action", action)
                .finish(),
            Self::DrawResponse {
                request_id,
                response,
                ..
            } => f
                .debug_struct("DrawResponse")
                .field("request_id", request_id)
                .field("response", response)
                .finish(),
            Self::VirtualAudioPolicyRequest {
                request_id, policy, ..
            } => f
                .debug_struct("VirtualAudioPolicyRequest")
                .field("request_id", request_id)
                .field("policy", policy)
                .finish(),
            Self::VirtualAudioPolicyResponse {
                request_id,
                error_code,
                ..
            } => f
                .debug_struct("VirtualAudioPolicyResponse")
                .field("request_id", request_id)
                .field("error_code", error_code)
                .finish(),
            Self::StateChange { seq, change, .. } => f
                .debug_struct("StateChange")
                .field("seq", seq)
                .field("has_state", &change.which.is_some())
                .finish(),
            Self::QueryState { seq, id, .. } => f
                .debug_struct("QueryState")
                .field("seq", seq)
                .field("id", id)
                .finish(),
        }
    }
}

pub fn extract_system_message(msg: &ControlMessage) -> Option<SystemMessage> {
    use SystemMessage::*;
    Some(match *msg {
        ControlMessage::RpcRequest {
            seq,
            timestamp,
            ref request,
        } => {
            let request_id = request.request_header.as_ref().map_or(0, |h| h.request_id);
            match request.which.as_ref()? {
                rpc_request::Which::PrivateScreenSetting(req) => PrivacyRequest {
                    seq,
                    timestamp,
                    request_id,
                    request: req.clone(),
                },
                rpc_request::Which::DrawReq(req) => DrawRequest {
                    seq,
                    timestamp,
                    request_id,
                    action: req.which.as_ref()?.into(),
                },
                rpc_request::Which::VirtualAudioDriverPolicyReq(req) => VirtualAudioPolicyRequest {
                    seq,
                    timestamp,
                    request_id,
                    policy: req.policy,
                },
                _ => return None,
            }
        }
        ControlMessage::RpcResponse {
            seq,
            timestamp,
            ref response,
        } => {
            let request_id = response
                .response_header
                .as_ref()
                .map_or(0, |h| h.request_id);
            match response.which.as_ref()? {
                rpc_response::Which::PrivateScreenSetting(resp) => PrivacyResponse {
                    seq,
                    timestamp,
                    request_id,
                    response: *resp,
                },
                rpc_response::Which::DrawResp(resp) => DrawResponse {
                    seq,
                    timestamp,
                    request_id,
                    response: *resp,
                },
                rpc_response::Which::VirtualAudioDriverPolicyRsp(resp) => {
                    VirtualAudioPolicyResponse {
                        seq,
                        timestamp,
                        request_id,
                        error_code: resp.error_code,
                    }
                }
                _ => return None,
            }
        }
        ControlMessage::SystemStateChange {
            seq,
            timestamp,
            ref change,
        } => StateChange {
            seq,
            timestamp,
            change: change.clone(),
        },
        ControlMessage::QuerySystemState {
            seq,
            timestamp,
            ref query,
        } => QueryState {
            seq,
            timestamp,
            id: query.id,
        },
        _ => return None,
    })
}

fn encode_rpc_request(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    which: rpc_request::Which,
) -> Vec<u8> {
    encode_message(&ControlMessage::RpcRequest {
        seq,
        timestamp,
        request: pb::RpcRequest {
            request_header: Some(pb::RequestHeader { request_id }),
            which: Some(which),
        },
    })
}
fn encode_rpc_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    which: rpc_response::Which,
) -> Vec<u8> {
    encode_message(&ControlMessage::RpcResponse {
        seq,
        timestamp,
        response: pb::RpcResponse {
            response_header: Some(pb::ResponseHeader { request_id }),
            which: Some(which),
        },
    })
}

pub fn encode_privacy_request(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    request: pb::PrivateScreenSettingRequest,
) -> Vec<u8> {
    encode_rpc_request(
        seq,
        timestamp,
        request_id,
        rpc_request::Which::PrivateScreenSetting(request),
    )
}
pub fn encode_privacy_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    response: pb::PrivateScreenSettingResponse,
) -> Vec<u8> {
    encode_rpc_response(
        seq,
        timestamp,
        request_id,
        rpc_response::Which::PrivateScreenSetting(response),
    )
}

pub(crate) fn encode_draw_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    response: pb::DrawResponse,
) -> Vec<u8> {
    encode_rpc_response(
        seq,
        timestamp,
        request_id,
        rpc_response::Which::DrawResp(response),
    )
}
pub fn encode_draw_stroke_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    error_code: i32,
) -> Vec<u8> {
    encode_draw_response(
        seq,
        timestamp,
        request_id,
        pb::DrawResponse {
            which: Some(draw_response::Which::StrokeResponse(
                pb::DrawStrokeResponse { error_code },
            )),
        },
    )
}
pub fn encode_draw_clear_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    error_code: i32,
) -> Vec<u8> {
    encode_draw_response(
        seq,
        timestamp,
        request_id,
        pb::DrawResponse {
            which: Some(draw_response::Which::ClearResponse(pb::DrawClearResponse {
                error_code,
            })),
        },
    )
}
pub fn encode_draw_toggle_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    error_code: i32,
) -> Vec<u8> {
    encode_draw_response(
        seq,
        timestamp,
        request_id,
        pb::DrawResponse {
            which: Some(draw_response::Which::ToggleResponse(
                pb::DrawToggleResponse { error_code },
            )),
        },
    )
}

pub fn encode_virtual_audio_policy_request(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    policy: i32,
) -> Vec<u8> {
    encode_rpc_request(
        seq,
        timestamp,
        request_id,
        rpc_request::Which::VirtualAudioDriverPolicyReq(pb::VirtualAudioDriverPolicyRequest {
            policy,
        }),
    )
}
pub fn encode_virtual_audio_policy_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
    error_code: i32,
) -> Vec<u8> {
    encode_rpc_response(
        seq,
        timestamp,
        request_id,
        rpc_response::Which::VirtualAudioDriverPolicyRsp(pb::VirtualAudioDriverPolicyResponse {
            error_code,
        }),
    )
}
/// Acknowledge a desktop viewer's microphone probe even when no receiver is present.
pub fn encode_virtual_audio_noop_probe_response(
    seq: i64,
    timestamp: i64,
    request_id: i64,
) -> Vec<u8> {
    encode_virtual_audio_policy_response(seq, timestamp, request_id, 0)
}

fn encode_state_change(seq: i64, timestamp: i64, which: system_state_change::Which) -> Vec<u8> {
    encode_message(&ControlMessage::SystemStateChange {
        seq,
        timestamp,
        change: pb::SystemStateChange { which: Some(which) },
    })
}
pub fn encode_private_screen_state(
    seq: i64,
    timestamp: i64,
    state: pb::PrivateScreenState,
) -> Vec<u8> {
    encode_state_change(
        seq,
        timestamp,
        system_state_change::Which::PrivateScreenState(state),
    )
}
pub fn encode_secure_desktop_state(
    seq: i64,
    timestamp: i64,
    state: pb::SecureDesktopState,
) -> Vec<u8> {
    encode_state_change(seq, timestamp, system_state_change::Which::SdState(state))
}
pub fn encode_permission_state(seq: i64, timestamp: i64, state: pb::PermissionState) -> Vec<u8> {
    encode_state_change(
        seq,
        timestamp,
        system_state_change::Which::PermissionState(state),
    )
}
pub fn encode_remote_assist_file_transfer_state(
    seq: i64,
    timestamp: i64,
    enabled: bool,
) -> Vec<u8> {
    encode_state_change(
        seq,
        timestamp,
        system_state_change::Which::RemoteAssistFileTransferState(
            pb::RemoteAssistFileTransferState { enabled },
        ),
    )
}
pub fn encode_query_system_state(seq: i64, timestamp: i64, id: i32) -> Vec<u8> {
    encode_message(&ControlMessage::QuerySystemState {
        seq,
        timestamp,
        query: pb::QuerySystemState { id },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::control::decode_message;

    fn decode(wire: &[u8]) -> SystemMessage {
        extract_system_message(&decode_message(wire).unwrap()).unwrap()
    }

    #[test]
    fn privacy_uses_the_controller_platform_and_raw_type() {
        use PrivacyAction::{Disable, Enable};
        use pb::PrivateScreenType::{PsEnhancedPrivacy, PsNormalPrivacy};
        let normal = Enable(PsNormalPrivacy);
        let enhanced = Enable(PsEnhancedPrivacy);
        for (platform, when_false) in [
            (Some(1), [Disable, Disable, normal, enhanced, Disable]),
            (Some(3), [Disable, Disable, Disable, enhanced, Disable]),
            (Some(2), [Disable; 5]),
            (Some(4), [Disable; 5]),
            (None, [Disable; 5]),
            (Some(99), [Disable; 5]),
        ] {
            for (enable, expected) in [
                (false, when_false),
                (true, [normal, normal, normal, enhanced, normal]),
            ] {
                for (privacy_type, expected) in [0, 1, 2, 3, 99].into_iter().zip(expected) {
                    let request = pb::PrivateScreenSettingRequest {
                        enable,
                        privacy_type,
                        ..Default::default()
                    };
                    assert_eq!(
                        normalize_privacy_request(&request, platform),
                        expected,
                        "platform {platform:?}, enable {enable}, type {privacy_type}"
                    );
                }
            }
        }
    }

    #[test]
    fn privacy_rpcs_preserve_source_without_logging_it() {
        let (seq, timestamp, request_id) = (10, 20, 30);
        let request = pb::PrivateScreenSettingRequest {
            enable: false,
            privacy_type: 3,
            setting_source: "private-source".into(),
        };
        let message = decode(&encode_privacy_request(
            seq,
            timestamp,
            request_id,
            request.clone(),
        ));
        assert!(!format!("{message:?}").contains("private-source"));
        assert_eq!(
            message,
            SystemMessage::PrivacyRequest {
                seq,
                timestamp,
                request_id,
                request
            }
        );
        let response = pb::PrivateScreenSettingResponse {
            error_code: 19,
            req_privacy_type: 3,
            cur_privacy_type: 2,
        };
        assert_eq!(
            decode(&encode_privacy_response(
                seq, timestamp, request_id, response
            )),
            SystemMessage::PrivacyResponse {
                seq,
                timestamp,
                request_id,
                response
            }
        );
        assert_eq!(PRIVACY_REPLY_CHANNEL, DataChannel::Text);
    }

    #[test]
    fn draw_requests_preserve_optional_fields_without_logging_coordinates() {
        let (seq, timestamp, request_id) = (10, 20, 30);
        let points = vec![pb::DrawPoint {
            x: 123.625,
            y: -987.25,
        }];
        for (which, action) in [
            (
                draw_request::Which::StrokeRequest(pb::DrawStrokeRequest {
                    stroke_id: 41,
                    points: points.clone(),
                    screen_id: Some(0),
                    line_width: Some(4.75),
                    color: Some(77),
                }),
                DrawAction::Stroke {
                    stroke_id: 41,
                    points,
                    screen_id: Some(0),
                    line_width: Some(4.75),
                    color: Some(77),
                },
            ),
            (
                draw_request::Which::ClearRequest(pb::DrawClearRequest {
                    clear_type: 2,
                    stroke_id: 41,
                    screen_id: None,
                }),
                DrawAction::Clear {
                    clear_type: 2,
                    stroke_id: 41,
                    screen_id: None,
                },
            ),
            (
                draw_request::Which::ToggleRequest(pb::DrawToggleRequest { enable: false }),
                DrawAction::Toggle { enable: false },
            ),
        ] {
            let wire = encode_rpc_request(
                seq,
                timestamp,
                request_id,
                rpc_request::Which::DrawReq(pb::DrawRequest { which: Some(which) }),
            );
            let message = decode(&wire);
            if matches!(action, DrawAction::Stroke { .. }) {
                for debug in [format!("{message:?}"), format!("{action:?}")] {
                    for hidden in ["123.625", "-987.25", "4.75", "77"] {
                        assert!(!debug.contains(hidden));
                    }
                    for shown in [
                        "points_len: 1",
                        "screen_id: Some(0)",
                        "has_line_width: true",
                        "has_color: true",
                    ] {
                        assert!(debug.contains(shown));
                    }
                }
            }
            assert_eq!(
                message,
                SystemMessage::DrawRequest {
                    seq,
                    timestamp,
                    request_id,
                    action
                }
            );
        }
        let missing = encode_rpc_request(
            seq,
            timestamp,
            request_id,
            rpc_request::Which::DrawReq(pb::DrawRequest { which: None }),
        );
        assert!(extract_system_message(&decode_message(&missing).unwrap()).is_none());
    }

    #[test]
    fn draw_replies_preserve_subtype_and_error_code_on_text_channel() {
        let (seq, timestamp, request_id, error_code) = (10, 20, 30, -5);
        for (wire, which) in [
            (
                encode_draw_stroke_response(seq, timestamp, request_id, error_code),
                draw_response::Which::StrokeResponse(pb::DrawStrokeResponse { error_code }),
            ),
            (
                encode_draw_clear_response(seq, timestamp, request_id, error_code),
                draw_response::Which::ClearResponse(pb::DrawClearResponse { error_code }),
            ),
            (
                encode_draw_toggle_response(seq, timestamp, request_id, error_code),
                draw_response::Which::ToggleResponse(pb::DrawToggleResponse { error_code }),
            ),
        ] {
            assert_eq!(
                decode(&wire),
                SystemMessage::DrawResponse {
                    seq,
                    timestamp,
                    request_id,
                    response: pb::DrawResponse { which: Some(which) }
                }
            );
        }
        assert_eq!(DRAW_REPLY_CHANNEL, DataChannel::Text);
    }

    #[test]
    fn stale_peer_cannot_clear_disable_or_reset_the_new_annotation_owner() {
        let mut owner = DrawOwnership::new();
        let clear = DrawAction::Clear {
            clear_type: 1,
            stroke_id: 0,
            screen_id: None,
        };
        let enable = DrawAction::Toggle { enable: true };
        let disable = DrawAction::Toggle { enable: false };
        let stroke = DrawAction::Stroke {
            stroke_id: 7,
            points: vec![],
            screen_id: None,
            line_width: None,
            color: None,
        };
        assert!(!owner.accept_or_take_over("old", &clear));
        assert!(owner.accept_or_take_over("old", &enable));
        assert!(owner.accept_or_take_over("new", &stroke));
        assert!(!owner.reset_peer("old"));
        assert!(!owner.accept_or_take_over("old", &clear));
        assert!(!owner.accept_or_take_over("old", &disable));
        assert!(owner.accept_or_take_over("new", &clear));
        assert!(owner.accept_or_take_over("new", &disable));
        assert!(!owner.reset_peer("new"));
        owner.take_over("a");
        assert!(owner.accept_or_take_over("b", &enable));
        assert!(!owner.reset_peer("a"));
        assert!(owner.reset_peer("b"));
    }

    #[test]
    fn virtual_audio_noop_acknowledges_probe_without_a_fault() {
        let (seq, timestamp, request_id) = (10, 20, 30);
        assert_eq!(
            decode(&encode_virtual_audio_policy_request(
                seq, timestamp, request_id, 6
            )),
            SystemMessage::VirtualAudioPolicyRequest {
                seq,
                timestamp,
                request_id,
                policy: 6
            }
        );
        for (wire, error_code) in [
            (
                encode_virtual_audio_policy_response(seq, timestamp, request_id, -9),
                -9,
            ),
            (
                encode_virtual_audio_noop_probe_response(seq, timestamp, request_id),
                0,
            ),
        ] {
            assert_eq!(
                decode(&wire),
                SystemMessage::VirtualAudioPolicyResponse {
                    seq,
                    timestamp,
                    request_id,
                    error_code
                }
            );
        }
    }

    #[test]
    fn state_changes_and_queries_roundtrip() {
        use system_state_change::Which;
        let (seq, timestamp) = (10, 20);
        let privacy = pb::PrivateScreenState {
            error_code: -1,
            req_privacy_type: 3,
            cur_privacy_type: 1,
        };
        let desktop = pb::SecureDesktopState {
            is_locked: true,
            pos_x: 97531,
            is_client_ui_ready: Some(false),
            ..Default::default()
        };
        let permission = pb::PermissionState {
            video_capture_allowed: true,
            floating_ball: true,
            ..Default::default()
        };
        for (wire, which) in [
            (
                encode_private_screen_state(seq, timestamp, privacy),
                Which::PrivateScreenState(privacy),
            ),
            (
                encode_secure_desktop_state(seq, timestamp, desktop),
                Which::SdState(desktop),
            ),
            (
                encode_permission_state(seq, timestamp, permission),
                Which::PermissionState(permission),
            ),
            (
                encode_remote_assist_file_transfer_state(seq, timestamp, true),
                Which::RemoteAssistFileTransferState(pb::RemoteAssistFileTransferState {
                    enabled: true,
                }),
            ),
        ] {
            let message = decode(&wire);
            assert!(!format!("{message:?}").contains("97531"));
            assert_eq!(
                message,
                SystemMessage::StateChange {
                    seq,
                    timestamp,
                    change: pb::SystemStateChange { which: Some(which) }
                }
            );
        }
        assert_eq!(
            decode(&encode_query_system_state(seq, timestamp, 1)),
            SystemMessage::QueryState {
                seq,
                timestamp,
                id: 1
            }
        );
    }
}
