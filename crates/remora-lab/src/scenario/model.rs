//! YAML scenario schema (`actors`, `steps`, `parallel`, `expect`, `invariants`).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::LabError;

/// Top-level scenario loaded from YAML.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_profile")]
    pub profile: String,
    #[serde(default)]
    pub actors: BTreeMap<String, ActorSpec>,
    #[serde(default)]
    pub fakecloud: Option<FakeCloudSpec>,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub invariants: Vec<Invariant>,
}

fn default_profile() -> String {
    "windows-4.48.2".to_owned()
}

impl Scenario {
    pub fn from_yaml_str(raw: &str) -> Result<Self, LabError> {
        Ok(serde_yaml_ng::from_str(raw)?)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, LabError> {
        let raw = fs::read_to_string(path.as_ref())?;
        Self::from_yaml_str(&raw)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    Host,
    Viewer,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActorSpec {
    pub kind: ActorKind,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
}

/// Optional script overrides used when running against [`crate::target::TargetSpec::FakeCloud`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FakeCloudSpec {
    #[serde(default)]
    pub http_overrides: Vec<HttpOverrideSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpOverrideSpec {
    pub method: String,
    pub path: String,
    #[serde(default = "default_ok_status")]
    pub status: u16,
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
}

fn default_ok_status() -> u16 {
    200
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Step {
    Parallel { parallel: Vec<ParallelBranch> },
    Single(Box<ActionStep>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ParallelBranch {
    Sequence(Vec<ActionStep>),
    Single(Box<ActionStep>),
}

impl ParallelBranch {
    pub fn steps(&self) -> &[ActionStep] {
        match self {
            Self::Sequence(list) => list,
            Self::Single(step) => std::slice::from_ref(step.as_ref()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActionStep {
    #[serde(default)]
    pub id: Option<String>,
    pub actor: String,
    #[serde(flatten)]
    pub action: StepAction,
    #[serde(default)]
    pub save: BTreeMap<String, String>,
    #[serde(default)]
    pub expect: Option<Expect>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum StepAction {
    DeviceInit {
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        sign: Option<bool>,
    },
    QrGenerate {
        #[serde(default)]
        device_id: Option<String>,
    },
    QrStatus {
        #[serde(default)]
        ticket: Option<String>,
        #[serde(default)]
        qrcode_jump_url: Option<String>,
        #[serde(default = "default_login_status")]
        login_status: u8,
    },
    LoginByQrcode {
        #[serde(default)]
        ticket: Option<String>,
        #[serde(default)]
        qrcode_jump_url: Option<String>,
    },
    Login,
    UserInfo {
        #[serde(default)]
        token: Option<String>,
    },
    UserLogout {
        #[serde(default)]
        token: Option<String>,
    },
    DeviceList {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        device_id: Option<String>,
    },
    DeviceGroups {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        device_id: Option<String>,
    },
    DevicePreferences {
        target: String,
        #[serde(default)]
        token: Option<String>,
    },
    MacControllable {
        controllable: bool,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        device_id: Option<String>,
    },
    DeviceShareInfo {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        device_id: Option<String>,
    },
    DeviceShareInfoRefresh {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        device_id: Option<String>,
    },
    DeviceShareCancel {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        device_id: Option<String>,
    },
    DeviceShareSaved {
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        token: Option<String>,
    },
    GuestCreate {
        #[serde(default)]
        device_id: Option<String>,
    },
    GuestRoomCreate,
    GuestShareInfo,
    GuestShareCancel,
    GuestUploadControlMode {
        control_id: String,
        allow_control: bool,
        control_mode: String,
    },
    GuestUploadSign {
        control_id: String,
        can_remote_control: bool,
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        salt: Option<String>,
        #[serde(default)]
        sign: Option<String>,
    },
    GuestConfirmation {
        control_id: String,
        allow: bool,
    },
    RoomShareControlMode {
        connect_id: String,
        #[serde(default)]
        token: Option<String>,
    },
    RoomJoinShareByCode {
        connect_id: String,
        connect_code: String,
        #[serde(default)]
        force_join: bool,
        #[serde(default)]
        token: Option<String>,
    },
    RoomJoinShareByConfirmation {
        connect_id: String,
        #[serde(default)]
        control_id: Option<String>,
        #[serde(default)]
        force_join: bool,
        #[serde(default)]
        token: Option<String>,
    },
    RoomShareCancelRemoteAssist {
        connect_id: String,
        #[serde(default)]
        control_id: Option<String>,
        #[serde(default)]
        token: Option<String>,
    },
    ReleaseFetchUpdate {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        platform: Option<u8>,
        #[serde(default)]
        version_name: Option<String>,
        #[serde(default)]
        token: Option<String>,
    },
    DeviceUnbind {
        target: String,
        #[serde(default)]
        token: Option<String>,
    },
    RoomCreate {
        #[serde(default)]
        device_id: Option<String>,
        #[serde(default)]
        token: Option<String>,
    },
    RoomJoin {
        target: String,
        #[serde(default)]
        force_join: bool,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        caller_device_id: Option<String>,
    },
    WsConnect {
        #[serde(default)]
        room_token: Option<String>,
        #[serde(default)]
        ws_url: Option<String>,
        #[serde(default)]
        auto_room_info: bool,
    },
    WsDisconnect,
    SendRoomInfo,
    SendRefreshReconnectKey,
    SendControl {
        #[serde(default)]
        app_control_id: Option<String>,
        #[serde(default = "default_capture_type")]
        capture_type: i32,
    },
    SendSoac {
        ice_id: String,
        #[serde(default)]
        client_id: Option<String>,
        kind: String,
        #[serde(default)]
        sdp: Option<String>,
        #[serde(default)]
        candidate: Option<String>,
    },
    SendForwardSetting {
        ice_id: String,
        #[serde(default)]
        client_id: Option<String>,
        data: Value,
    },
    WaitEvent {
        event: String,
        #[serde(default = "default_wait_timeout_ms")]
        timeout_ms: u64,
    },
    PollDeviceStatus {
        target: String,
        expected_status: String,
        #[serde(default = "default_wait_timeout_ms")]
        timeout_ms: u64,
    },
    RawHttp {
        method: String,
        path: String,
        #[serde(default)]
        body: Option<Value>,
        #[serde(default)]
        raw_body: Option<String>,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        device_id: Option<String>,
        #[serde(default = "default_true")]
        sign: bool,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    SleepMs {
        ms: u64,
    },
    HeadlessRoundtrip {
        #[serde(default = "default_frame_count")]
        expected_frames: u64,
    },
    SessionHostStart {
        #[serde(default)]
        force_relay: bool,
        #[serde(default)]
        prefer_relay: bool,
        #[serde(default)]
        reject_reason: Option<String>,
        #[serde(default = "default_true")]
        loop_video: bool,
    },
    SessionViewerConnect {
        target: String,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        force_relay: bool,
        #[serde(default)]
        prefer_relay: bool,
        #[serde(default)]
        force_join: bool,
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionSendInput {
        kind: String,
        #[serde(default)]
        key: Option<i32>,
        #[serde(default)]
        button: Option<i32>,
        #[serde(default)]
        x: Option<f64>,
        #[serde(default)]
        y: Option<f64>,
        #[serde(default)]
        dx: Option<i32>,
        #[serde(default)]
        dy: Option<i32>,
        #[serde(default)]
        text: Option<String>,
    },
    SessionSetClipboard {
        text: String,
    },
    SessionWait {
        #[serde(default)]
        min_frames: Option<u64>,
        #[serde(default)]
        min_inputs: Option<u64>,
        #[serde(default)]
        expected_clipboard: Option<String>,
        #[serde(default)]
        require_relayed: Option<bool>,
        #[serde(default)]
        require_p2p: Option<bool>,
        #[serde(default)]
        require_tool_channels: Option<bool>,
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionFileReadDir {
        path: String,
        #[serde(default = "default_task_id")]
        task_id: i32,
        #[serde(default)]
        channel: Option<String>,
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionFileBlock {
        #[serde(default = "default_task_id")]
        task_id: i32,
        #[serde(default)]
        file_index: i32,
        #[serde(default)]
        block_id: i32,
        data: String,
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionFileRemoveDir {
        path: String,
        #[serde(default = "default_task_id")]
        task_id: i32,
        #[serde(default)]
        recursive: bool,
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionTerminalCheckEnv {
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionTerminalRoundtrip {
        #[serde(default = "default_term_session_id")]
        session_id: u32,
        input: String,
        #[serde(default)]
        close_header_session_id: Option<u32>,
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionTunnelRoundtrip {
        #[serde(default = "default_u64_one")]
        session_id: u64,
        #[serde(default = "default_u64_one")]
        rule_id: u64,
        #[serde(default = "default_u64_one")]
        stream_id: u64,
        target_host: String,
        target_port: u16,
        data: String,
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionSystemPrivacy {
        enable: bool,
        privacy_type: i32,
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionSystemDraw {
        enable: bool,
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionSystemVirtualAudioProbe {
        #[serde(default)]
        policy: i32,
        #[serde(default = "default_session_timeout_ms")]
        timeout_ms: u64,
    },
    SessionClose,
}

fn default_login_status() -> u8 {
    1
}

fn default_capture_type() -> i32 {
    1
}

fn default_wait_timeout_ms() -> u64 {
    3_000
}

fn default_true() -> bool {
    true
}

fn default_frame_count() -> u64 {
    3
}

fn default_session_timeout_ms() -> u64 {
    10_000
}

fn default_task_id() -> i32 {
    1
}

fn default_term_session_id() -> u32 {
    1
}

fn default_u64_one() -> u64 {
    1
}

/// Assertions evaluated after a step finishes.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub http_status: Option<u16>,
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub ack_status: Option<String>,
    #[serde(default)]
    pub event_name: Option<String>,
    #[serde(default)]
    pub json_eq: BTreeMap<String, Value>,
    #[serde(default)]
    pub json_exists: Vec<String>,
    #[serde(default)]
    pub json_contains: BTreeMap<String, Value>,
}

/// Global invariants verified across the full scenario execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Invariant {
    NoSecretsInLogs,
    NoSecretsInTrace,
    RedactedTrace,
}
