//! Executes [`Scenario`] definitions against a [`RunningTarget`] and verifies expectations and invariants.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use prost::Message as _;
use rand::SeedableRng;
use remora_endpoint::Endpoint;
use remora_endpoint::client::driver::drive_machine_with;
use remora_endpoint::client::http::{ControlClient, HttpError};
use remora_endpoint::client::socket::SignalingSocket;
use remora_endpoint::platform::Platform;
use remora_endpoint::record::{TraceActor, TraceRecord, TraceRecorder};
use remora_endpoint::role::{
    AuthorizationDecision, HostConfig, HostSession, ViewerConfig, ViewerSession,
};
use remora_proto::control::signing::{PARAM_PREFIX, SIGN_HEADER, SignKey, sign};
use remora_proto::control::{
    ApiError, DeviceInitRequest, LoginByQrcodeRequest, QrCodeStatus, QrLoginStatusRequest,
    RoomJoinRequest,
};
use remora_proto::machine::Timestamp;
use remora_proto::machine::signaling::{SignalingEvent, SignalingMachine};
use remora_proto::pb;
use remora_proto::profile::{self, Profile};
use remora_proto::session::input::InputMessage;
use remora_proto::signal::{ControlAck, SignalEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;
use url::Url;

use crate::error::LabError;
use crate::headless::{
    AnnexBFileVideoSource, HeadlessPlatform, MemoryClipboard, RecordingInputSink,
    ScriptedInputSource, synthetic_annex_b_sample,
};
use crate::redact::{find_secret_leak, redact_value};
use crate::scenario::model::{
    ActionStep, ActorKind, Expect, Invariant, Scenario, Step, StepAction,
};
use crate::target::RunningTarget;

/// Outcome of a single executed step, kept for reporting, normalization, and diffing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedStep {
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub actor: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_name: Option<String>,
    pub payload: Value,
}

/// Complete outcome of running one [`Scenario`] against one target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub scenario: String,
    pub target: String,
    pub passed: bool,
    pub error: Option<String>,
    pub steps: Vec<ObservedStep>,
    pub trace: Vec<TraceRecord>,
    pub duration_ms: u64,
}

#[derive(Clone, Default)]
struct SharedTraceBuffer(Arc<StdMutex<Vec<u8>>>);

impl std::io::Write for SharedTraceBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("trace buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedTraceBuffer {
    fn records(&self) -> Vec<TraceRecord> {
        let bytes = self.0.lock().expect("trace buffer poisoned").clone();
        let text = String::from_utf8_lossy(&bytes);
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<TraceRecord>(line).ok())
            .collect()
    }
}

enum WsCmd {
    SendRoomInfo,
    SendRefreshReconnectKey,
    SendControl {
        header: Value,
        connect_options: Vec<u8>,
    },
    SendSoac(Value),
    SendForwardSetting(Value),
    Close,
}

struct WsHandle {
    cmd_tx: mpsc::Sender<WsCmd>,
    events: Arc<Mutex<VecDeque<SignalingEvent>>>,
    notify: Arc<Notify>,
    task: JoinHandle<()>,
}

impl WsHandle {
    async fn close(self) {
        let _ = self.cmd_tx.send(WsCmd::Close).await;
        let _ = tokio::time::timeout(Duration::from_millis(250), self.task).await;
    }

    async fn wait_for<F>(&self, timeout: Duration, mut pred: F) -> Result<SignalingEvent, LabError>
    where
        F: FnMut(&SignalingEvent) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            {
                let mut q = self.events.lock().await;
                if let Some(pos) = q.iter().position(&mut pred) {
                    return Ok(q.remove(pos).expect("position exists"));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let pending: Vec<String> = self
                    .events
                    .lock()
                    .await
                    .iter()
                    .map(|e| format!("{e:?}"))
                    .collect();
                return Err(LabError::Scenario(format!(
                    "timed out waiting for signaling event after {}ms; buffered={pending:?}",
                    timeout.as_millis()
                )));
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }
}

struct ActorState {
    kind: ActorKind,
    profile: &'static Profile,
    client_id: String,
    device_name: String,
    http: ControlClient,
    raw_client: reqwest::Client,
    recorder: Arc<TraceRecorder>,
    device_id: Option<String>,
    user_token: Option<String>,
    qr_ticket: Option<String>,
    qr_jump_url: Option<String>,
    room_token: Option<String>,
    ws_url: Option<String>,
    last_ice_id: Option<String>,
    last_peer_client_id: Option<String>,
    ws: Option<WsHandle>,
    host_session: Option<HostSession>,
    viewer_session: Option<ViewerSession>,
    input_sink: Option<RecordingInputSink>,
    clipboard_mirror: Option<MemoryClipboard>,
    sent_inputs: Vec<InputMessage>,
}

struct SharedEnv {
    base_url: String,
    sign_key: Option<SignKey>,
    actors: BTreeMap<String, Arc<Mutex<ActorState>>>,
    vars: Mutex<BTreeMap<String, String>>,
    secrets: StdMutex<Vec<(String, String)>>,
    observed: Mutex<Vec<(usize, usize, ObservedStep)>>,
    step_seq: StdMutex<usize>,
    trace_buf: SharedTraceBuffer,
}

impl SharedEnv {
    fn record_secret(&self, label: impl Into<String>, value: &str) {
        if !value.is_empty() {
            self.secrets
                .lock()
                .expect("secrets lock poisoned")
                .push((label.into(), value.to_owned()));
        }
    }

    fn next_step_index(&self) -> usize {
        let mut guard = self.step_seq.lock().expect("seq lock poisoned");
        let idx = *guard;
        *guard += 1;
        idx
    }

    async fn resolve_template(&self, input: &str) -> String {
        if !input.contains("${") {
            return input.to_owned();
        }
        let mut out = input.to_owned();
        let vars = self.vars.lock().await.clone();
        for (k, v) in &vars {
            out = out.replace(&format!("${{vars.{k}}}"), v);
        }
        for (name, actor_mutex) in &self.actors {
            let actor = actor_mutex.lock().await;
            if let Some(id) = &actor.device_id {
                out = out.replace(&format!("${{{name}.device_id}}"), id);
            }
            if let Some(tok) = &actor.user_token {
                out = out.replace(&format!("${{{name}.user_token}}"), tok);
            }
            if let Some(tok) = &actor.room_token {
                out = out.replace(&format!("${{{name}.room_token}}"), tok);
            }
            if let Some(ticket) = &actor.qr_ticket {
                out = out.replace(&format!("${{{name}.qr_ticket}}"), ticket);
            }
            if let Some(url) = &actor.qr_jump_url {
                out = out.replace(&format!("${{{name}.qr_jump_url}}"), url);
            }
            if let Some(ice_id) = &actor.last_ice_id {
                out = out.replace(&format!("${{{name}.last_ice_id}}"), ice_id);
            }
            if let Some(peer_id) = &actor.last_peer_client_id {
                out = out.replace(&format!("${{{name}.last_peer_client_id}}"), peer_id);
            }
            out = out.replace(&format!("${{{name}.client_id}}"), &actor.client_id);
        }
        out
    }

    async fn resolve_json(&self, val: &Value) -> Value {
        match val {
            Value::String(s) => Value::String(self.resolve_template(s).await),
            Value::Array(arr) => {
                let mut out = Vec::with_capacity(arr.len());
                for item in arr {
                    out.push(Box::pin(self.resolve_json(item)).await);
                }
                Value::Array(out)
            }
            Value::Object(map) => {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (k, v) in map {
                    out.insert(k.clone(), Box::pin(self.resolve_json(v)).await);
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
}

/// Runs a single [`Scenario`] against `target` and returns its structured [`ScenarioResult`].
pub async fn run_scenario(
    scenario: &Scenario,
    target: &RunningTarget,
    target_label: &str,
) -> ScenarioResult {
    let start = Instant::now();
    let base_url = target.base_url().to_owned();
    let sign_key = target.sign_key().cloned();
    let trace_buf = SharedTraceBuffer::default();

    let mut actors = BTreeMap::new();
    let default_profile_id = scenario.profile.as_str();
    let raw_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client builds");

    for (name, spec) in &scenario.actors {
        let pid = spec.profile.as_deref().unwrap_or(default_profile_id);
        let Some(prof) = profile::by_id(pid) else {
            return ScenarioResult {
                scenario: scenario.name.clone(),
                target: target_label.to_owned(),
                passed: false,
                error: Some(format!("unknown profile `{pid}` for actor `{name}`")),
                steps: Vec::new(),
                trace: Vec::new(),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        };
        let trace_actor = match spec.kind {
            ActorKind::Host => TraceActor::Host,
            ActorKind::Viewer => TraceActor::Viewer,
        };
        let recorder = Arc::new(TraceRecorder::new(trace_actor, trace_buf.clone()));
        let parsed_url = match Url::parse(&base_url) {
            Ok(u) => u,
            Err(err) => {
                return ScenarioResult {
                    scenario: scenario.name.clone(),
                    target: target_label.to_owned(),
                    passed: false,
                    error: Some(format!("invalid base URL `{base_url}`: {err}")),
                    steps: Vec::new(),
                    trace: Vec::new(),
                    duration_ms: start.elapsed().as_millis() as u64,
                };
            }
        };
        let client_key = sign_key.clone().unwrap_or_else(|| SignKey::new(""));
        let http =
            ControlClient::with_http_client(parsed_url, prof, client_key, raw_client.clone())
                .with_recorder(Arc::clone(&recorder));

        let client_id = spec
            .client_id
            .clone()
            .unwrap_or_else(|| format!("lab-{}-{name}", scenario.name));
        let device_name = spec.device_name.clone().unwrap_or_else(|| name.clone());

        actors.insert(
            name.clone(),
            Arc::new(Mutex::new(ActorState {
                kind: spec.kind,
                profile: prof,
                client_id,
                device_name,
                http,
                raw_client: raw_client.clone(),
                recorder,
                device_id: None,
                user_token: None,
                qr_ticket: None,
                qr_jump_url: None,
                room_token: None,
                ws_url: None,
                last_ice_id: None,
                last_peer_client_id: None,
                ws: None,
                host_session: None,
                viewer_session: None,
                input_sink: None,
                clipboard_mirror: None,
                sent_inputs: Vec::new(),
            })),
        );
    }

    let env = Arc::new(SharedEnv {
        base_url,
        sign_key,
        actors,
        vars: Mutex::new(BTreeMap::new()),
        secrets: StdMutex::new(Vec::new()),
        observed: Mutex::new(Vec::new()),
        step_seq: StdMutex::new(0),
        trace_buf: trace_buf.clone(),
    });

    let mut run_err: Option<String> = None;
    for step in &scenario.steps {
        if let Err(err) = execute_step(&env, step).await {
            run_err = Some(err.to_string());
            break;
        }
    }

    // Close any open sessions and WebSockets cleanly before reading final logs/traces.
    for actor_mutex in env.actors.values() {
        let (ws, viewer_sess, host_sess) = {
            let mut guard = actor_mutex.lock().await;
            (
                guard.ws.take(),
                guard.viewer_session.take(),
                guard.host_session.take(),
            )
        };
        if let Some(sess) = viewer_sess {
            sess.close().await;
        }
        if let Some(sess) = host_sess {
            sess.close().await;
        }
        if let Some(handle) = ws {
            handle.close().await;
        }
    }

    let trace = env.trace_buf.records();
    if run_err.is_none()
        && let Err(err) = check_invariants(&scenario.invariants, &env, target, &trace)
    {
        run_err = Some(err.to_string());
    }

    let mut raw_steps = env.observed.lock().await.clone();
    raw_steps.sort_by_key(|(primary, secondary, _)| (*primary, *secondary));
    let steps = raw_steps
        .into_iter()
        .enumerate()
        .map(|(idx, (_, _, mut step))| {
            step.index = idx;
            step
        })
        .collect();

    ScenarioResult {
        scenario: scenario.name.clone(),
        target: target_label.to_owned(),
        passed: run_err.is_none(),
        error: run_err,
        steps,
        trace,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

fn check_invariants(
    invariants: &[Invariant],
    env: &SharedEnv,
    target: &RunningTarget,
    trace: &[TraceRecord],
) -> Result<(), LabError> {
    let secrets = env.secrets.lock().expect("secrets lock poisoned").clone();
    for inv in invariants {
        match inv {
            Invariant::NoSecretsInLogs => {
                if let Some(leaked) = target.find_log_secret_leak(&secrets) {
                    return Err(LabError::Scenario(format!(
                        "invariant no_secrets_in_logs violated: `{leaked}` appeared in server logs"
                    )));
                }
            }
            Invariant::NoSecretsInTrace => {
                let raw_trace = serde_json::to_string(trace)?;
                if let Some(leaked) = find_secret_leak(&raw_trace, &secrets) {
                    return Err(LabError::Scenario(format!(
                        "invariant no_secrets_in_trace violated: `{leaked}` appeared in trace"
                    )));
                }
            }
            Invariant::RedactedTrace => {
                if trace.iter().any(|r| !r.redacted) {
                    return Err(LabError::Scenario(
                        "invariant redacted_trace violated: unredacted record found".into(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn execute_step<'a>(
    env: &'a Arc<SharedEnv>,
    step: &'a Step,
) -> Pin<Box<dyn Future<Output = Result<(), LabError>> + Send + 'a>> {
    Box::pin(async move {
        match step {
            Step::Single(action_step) => {
                let primary = env.next_step_index();
                execute_action_step(env, action_step, primary, 0).await
            }
            Step::Parallel { parallel } => {
                let primary = env.next_step_index();
                let mut tasks = Vec::with_capacity(parallel.len());
                for (branch_idx, branch) in parallel.iter().enumerate() {
                    let env = Arc::clone(env);
                    let steps = branch.steps().to_vec();
                    tasks.push(tokio::spawn(async move {
                        for (sub_idx, item) in steps.iter().enumerate() {
                            execute_action_step(&env, item, primary, branch_idx * 1024 + sub_idx)
                                .await?;
                        }
                        Ok::<(), LabError>(())
                    }));
                }
                for handle in tasks {
                    handle.await.map_err(|e| {
                        LabError::Scenario(format!("parallel branch panicked: {e}"))
                    })??;
                }
                Ok(())
            }
        }
    })
}

async fn execute_action_step(
    env: &Arc<SharedEnv>,
    step: &ActionStep,
    primary_idx: usize,
    secondary_idx: usize,
) -> Result<(), LabError> {
    let step_idx = primary_idx;
    let actor_mutex = env
        .actors
        .get(&step.actor)
        .ok_or_else(|| LabError::Scenario(format!("unknown actor `{}`", step.actor)))?
        .clone();

    let (action_label, outcome) = match &step.action {
        StepAction::DeviceInit {
            client_id,
            name,
            sign,
        } => {
            let resolved_cid = match client_id {
                Some(c) => Some(env.resolve_template(c).await),
                None => None,
            };
            let resolved_name = match name {
                Some(n) => Some(env.resolve_template(n).await),
                None => None,
            };
            let mut actor = actor_mutex.lock().await;
            let cid = resolved_cid.unwrap_or_else(|| actor.client_id.clone());
            let dname = resolved_name.unwrap_or_else(|| actor.device_name.clone());
            if sign == &Some(false) {
                let path = actor.profile.init_endpoint.clone();
                let resp = send_raw_http(
                    &actor.raw_client,
                    &env.base_url,
                    env.sign_key.as_ref(),
                    "POST",
                    &path,
                    Some(json!({ "client_id": cid, "name": dname })),
                    None,
                    None,
                    None,
                    false,
                    &BTreeMap::new(),
                    &actor.recorder,
                )
                .await?;
                ("device_init", resp)
            } else {
                let res = actor
                    .http
                    .device_init(&DeviceInitRequest {
                        client_id: cid,
                        name: dname,
                    })
                    .await;
                let out = match res {
                    Ok(ok) => {
                        actor.device_id = Some(ok.device_id.clone());
                        StepOutcome::http_ok(serde_json::to_value(ok)?)
                    }
                    Err(err) => StepOutcome::from_http_err(err)?,
                };
                ("device_init", out)
            }
        }
        StepAction::QrGenerate { device_id } => {
            let explicit_dev = match device_id {
                Some(d) => Some(env.resolve_template(d).await),
                None => None,
            };
            let mut actor = actor_mutex.lock().await;
            let dev = explicit_dev.or_else(|| actor.device_id.clone());
            let out = match actor.http.qr_login_generate(dev.as_deref()).await {
                Ok(resp) => {
                    env.record_secret("qr_jump_url", &resp.qrcode_jump_url);
                    env.record_secret("qr_ticket", &resp.status_query_ticket);
                    actor.qr_jump_url = Some(resp.qrcode_jump_url.clone());
                    actor.qr_ticket = Some(resp.status_query_ticket.clone());
                    StepOutcome::http_ok(serde_json::to_value(resp)?)
                }
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("qr_generate", out)
        }
        StepAction::QrStatus {
            ticket,
            qrcode_jump_url,
            login_status,
        } => {
            let t = match ticket {
                Some(s) => Some(env.resolve_template(s).await),
                None => None,
            };
            let u = match qrcode_jump_url {
                Some(s) => Some(env.resolve_template(s).await),
                None => None,
            };
            let actor = actor_mutex.lock().await;
            let req = QrLoginStatusRequest {
                token: t
                    .or_else(|| actor.qr_ticket.clone())
                    .ok_or_else(|| LabError::Scenario("missing qr ticket".into()))?,
                qrcode_jump_url: u
                    .or_else(|| actor.qr_jump_url.clone())
                    .ok_or_else(|| LabError::Scenario("missing qrcode_jump_url".into()))?,
                login_status: *login_status,
            };
            let out = match actor.http.qr_login_status(&req).await {
                Ok(resp) => StepOutcome::http_ok(serde_json::to_value(resp)?),
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("qr_status", out)
        }
        StepAction::LoginByQrcode {
            ticket,
            qrcode_jump_url,
        } => {
            let t = match ticket {
                Some(s) => Some(env.resolve_template(s).await),
                None => None,
            };
            let u = match qrcode_jump_url {
                Some(s) => Some(env.resolve_template(s).await),
                None => None,
            };
            let mut actor = actor_mutex.lock().await;
            let req = LoginByQrcodeRequest {
                token: t
                    .or_else(|| actor.qr_ticket.clone())
                    .ok_or_else(|| LabError::Scenario("missing qr ticket".into()))?,
                qrcode_jump_url: u
                    .or_else(|| actor.qr_jump_url.clone())
                    .ok_or_else(|| LabError::Scenario("missing qrcode_jump_url".into()))?,
            };
            let out = match actor.http.login_by_qrcode(&req).await {
                Ok(resp) => {
                    env.record_secret("user_token", &resp.token);
                    actor.user_token = Some(resp.token.clone());
                    StepOutcome::http_ok(serde_json::to_value(resp)?)
                }
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("login_by_qrcode", out)
        }
        StepAction::Login => {
            let mut actor = actor_mutex.lock().await;
            let dev = actor.device_id.clone();
            let qr = actor.http.qr_login_generate(dev.as_deref()).await?;
            env.record_secret("qr_jump_url", &qr.qrcode_jump_url);
            env.record_secret("qr_ticket", &qr.status_query_ticket);
            actor.qr_jump_url = Some(qr.qrcode_jump_url.clone());
            actor.qr_ticket = Some(qr.status_query_ticket.clone());
            let st = actor
                .http
                .qr_login_status(&QrLoginStatusRequest {
                    token: qr.status_query_ticket.clone(),
                    qrcode_jump_url: qr.qrcode_jump_url.clone(),
                    login_status: u8::from(QrCodeStatus::Waiting),
                })
                .await?;
            if QrCodeStatus::from(st.status) != QrCodeStatus::Confirmed {
                return Err(LabError::Scenario(format!(
                    "expected confirmed QR status (4), got {}",
                    st.status
                )));
            }
            let out = match actor
                .http
                .login_by_qrcode(&LoginByQrcodeRequest {
                    token: qr.status_query_ticket,
                    qrcode_jump_url: qr.qrcode_jump_url,
                })
                .await
            {
                Ok(resp) => {
                    env.record_secret("user_token", &resp.token);
                    actor.user_token = Some(resp.token.clone());
                    StepOutcome::http_ok(serde_json::to_value(resp)?)
                }
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("login", out)
        }
        StepAction::UserInfo { token } => {
            let explicit_tok = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let actor = actor_mutex.lock().await;
            let tok = explicit_tok
                .or_else(|| actor.user_token.clone())
                .unwrap_or_default();
            let out = match actor.http.user_info(&tok, actor.device_id.as_deref()).await {
                Ok(resp) => StepOutcome::http_ok(serde_json::to_value(resp)?),
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("user_info", out)
        }
        StepAction::UserLogout { token } => {
            let explicit_tok = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let actor = actor_mutex.lock().await;
            let tok = explicit_tok
                .or_else(|| actor.user_token.clone())
                .unwrap_or_default();
            let out = match actor
                .http
                .user_logout(&tok, actor.device_id.as_deref())
                .await
            {
                Ok(_) => StepOutcome::http_ok(json!({})),
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("user_logout", out)
        }
        StepAction::DeviceList { token, device_id } => {
            let explicit_tok = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let explicit_dev = match device_id {
                Some(d) => Some(env.resolve_template(d).await),
                None => None,
            };
            let actor = actor_mutex.lock().await;
            let tok = explicit_tok
                .or_else(|| actor.user_token.clone())
                .unwrap_or_default();
            let dev = explicit_dev.or_else(|| actor.device_id.clone());
            let out = match actor.http.device_list(&tok, dev.as_deref()).await {
                Ok(resp) => StepOutcome::http_ok(serde_json::to_value(resp)?),
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("device_list", out)
        }
        StepAction::DeviceGroups { token, device_id } => {
            let explicit_tok = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let explicit_dev = match device_id {
                Some(d) => Some(env.resolve_template(d).await),
                None => None,
            };
            let actor = actor_mutex.lock().await;
            let tok = explicit_tok
                .or_else(|| actor.user_token.clone())
                .unwrap_or_default();
            let dev = explicit_dev.or_else(|| actor.device_id.clone());
            let out = match actor.http.device_groups(&tok, dev.as_deref()).await {
                Ok(resp) => StepOutcome::http_ok(serde_json::to_value(resp)?),
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("device_groups", out)
        }
        StepAction::DevicePreferences { target, token } => {
            let target_id = resolve_target_device(env, target).await?;
            let explicit_tok = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let actor = actor_mutex.lock().await;
            let tok = explicit_tok
                .or_else(|| actor.user_token.clone())
                .unwrap_or_default();
            let out = match actor
                .http
                .device_preferences(&tok, &target_id, actor.device_id.as_deref())
                .await
            {
                Ok(resp) => StepOutcome::http_ok(serde_json::to_value(resp)?),
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("device_preferences", out)
        }
        StepAction::MacControllable {
            controllable,
            token,
            device_id,
        } => {
            let explicit_tok = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let explicit_dev = match device_id {
                Some(d) => Some(env.resolve_template(d).await),
                None => None,
            };
            let actor = actor_mutex.lock().await;
            let tok = explicit_tok
                .or_else(|| actor.user_token.clone())
                .unwrap_or_default();
            let dev = explicit_dev
                .or_else(|| actor.device_id.clone())
                .unwrap_or_default();
            let out = match actor.http.mac_controllable(&tok, &dev, *controllable).await {
                Ok(_) => StepOutcome::http_ok(json!({})),
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("mac_controllable", out)
        }
        StepAction::DeviceUnbind { target, token } => {
            let target_id = resolve_target_device(env, target).await?;
            let explicit_tok = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let actor = actor_mutex.lock().await;
            let tok = explicit_tok
                .or_else(|| actor.user_token.clone())
                .unwrap_or_default();
            let out = match actor
                .http
                .device_unbind(&tok, &target_id, actor.device_id.as_deref())
                .await
            {
                Ok(_) => StepOutcome::http_ok(json!({})),
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("device_unbind", out)
        }
        StepAction::RoomCreate { device_id, token } => {
            let explicit_dev = match device_id {
                Some(d) => Some(env.resolve_template(d).await),
                None => None,
            };
            let explicit_tok = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let mut actor = actor_mutex.lock().await;
            let dev = explicit_dev
                .or_else(|| actor.device_id.clone())
                .unwrap_or_default();
            let tok = explicit_tok
                .or_else(|| actor.user_token.clone())
                .unwrap_or_default();
            let out = match actor.http.room_create(&tok, &dev).await {
                Ok(resp) => {
                    env.record_secret("room_token", &resp.token);
                    let ws_url = resp
                        .signaling_list
                        .first()
                        .filter(|u| !u.is_empty())
                        .cloned()
                        .unwrap_or_else(|| resp.signaling_server.clone());
                    actor.room_token = Some(resp.token.clone());
                    actor.ws_url = Some(ws_url);
                    StepOutcome::http_ok(serde_json::to_value(resp)?)
                }
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("room_create", out)
        }
        StepAction::RoomJoin {
            target,
            force_join,
            token,
            caller_device_id,
        } => {
            let target_id = resolve_target_device(env, target).await?;
            let explicit_tok = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let explicit_caller = match caller_device_id {
                Some(d) => Some(env.resolve_template(d).await),
                None => None,
            };
            let mut actor = actor_mutex.lock().await;
            let tok = explicit_tok
                .or_else(|| actor.user_token.clone())
                .unwrap_or_default();
            let caller = explicit_caller.or_else(|| actor.device_id.clone());
            let out = match actor
                .http
                .room_join_by_device(
                    &tok,
                    &target_id,
                    caller.as_deref(),
                    &RoomJoinRequest {
                        force_join: *force_join,
                    },
                )
                .await
            {
                Ok(resp) => {
                    env.record_secret("room_token", &resp.token);
                    let ws_url = resp.signaling_list.first().cloned().unwrap_or_default();
                    actor.room_token = Some(resp.token.clone());
                    actor.ws_url = Some(ws_url);
                    StepOutcome::http_ok(serde_json::to_value(resp)?)
                }
                Err(err) => StepOutcome::from_http_err(err)?,
            };
            ("room_join", out)
        }
        StepAction::WsConnect {
            room_token,
            ws_url,
            auto_room_info,
        } => {
            let explicit_tok = match room_token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let explicit_url = match ws_url {
                Some(u) => Some(env.resolve_template(u).await),
                None => None,
            };
            let (profile, r_tok, url, rec, old_ws, kind) = {
                let mut actor = actor_mutex.lock().await;
                let r_tok = explicit_tok
                    .or_else(|| actor.room_token.clone())
                    .ok_or_else(|| {
                        LabError::Scenario("missing room_token for ws_connect".into())
                    })?;
                let url = explicit_url
                    .or_else(|| actor.ws_url.clone())
                    .ok_or_else(|| LabError::Scenario("missing ws_url for ws_connect".into()))?;
                (
                    actor.profile,
                    r_tok,
                    url,
                    Arc::clone(&actor.recorder),
                    actor.ws.take(),
                    actor.kind,
                )
            };
            if let Some(prev) = old_ws {
                prev.close().await;
            }
            let handle = spawn_ws_handle(url, r_tok, profile, rec).await?;
            handle
                .wait_for(Duration::from_secs(5), |ev| {
                    matches!(
                        ev,
                        SignalingEvent::Connected | SignalingEvent::ConnectError(_)
                    )
                })
                .await?;
            if *auto_room_info && kind == ActorKind::Host {
                let _ = handle.cmd_tx.send(WsCmd::SendRoomInfo).await;
            }
            actor_mutex.lock().await.ws = Some(handle);
            (
                "ws_connect",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("connected".into()),
                    payload: json!({ "connected": true }),
                },
            )
        }
        StepAction::WsDisconnect => {
            let ws = actor_mutex.lock().await.ws.take();
            if let Some(handle) = ws {
                handle.close().await;
            }
            (
                "ws_disconnect",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("disconnected".into()),
                    payload: json!({ "disconnected": true }),
                },
            )
        }
        StepAction::SendRoomInfo => {
            let wait_res = {
                let mut actor = actor_mutex.lock().await;
                let handle = actor
                    .ws
                    .take()
                    .ok_or_else(|| LabError::Scenario("actor has no active WebSocket".into()))?;
                let _ = handle.cmd_tx.send(WsCmd::SendRoomInfo).await;
                let res = handle
                    .wait_for(Duration::from_secs(3), |ev| {
                        matches!(ev, SignalingEvent::Ack { .. })
                    })
                    .await;
                actor.ws = Some(handle);
                res?
            };
            let args = match wait_res {
                SignalingEvent::Ack { args, .. } => Value::Array(args),
                _ => Value::Null,
            };
            (
                "send_room_info",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: Some("ok".into()),
                    event_name: Some("room_info_ack".into()),
                    payload: args,
                },
            )
        }
        StepAction::SendRefreshReconnectKey => {
            let ev = {
                let actor = actor_mutex.lock().await;
                let handle = actor
                    .ws
                    .as_ref()
                    .ok_or_else(|| LabError::Scenario("actor has no active WebSocket".into()))?;
                let _ = handle.cmd_tx.send(WsCmd::SendRefreshReconnectKey).await;
                handle
                    .wait_for(Duration::from_secs(3), |ev| {
                        matches!(ev, SignalingEvent::ReconnectKeyRefreshed)
                    })
                    .await?
            };
            let _ = ev;
            (
                "send_refresh_reconnect_key",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: Some("success".into()),
                    event_name: Some("refresh_reconnect_key_ack".into()),
                    payload: json!({ "refreshed": true }),
                },
            )
        }
        StepAction::SendControl {
            app_control_id,
            capture_type,
        } => {
            let app_id = match app_control_id {
                Some(s) => Some(env.resolve_template(s).await),
                None => None,
            };
            let opts = pb::ConnectOptions {
                capture_type: *capture_type,
                ..Default::default()
            }
            .encode_to_vec();
            let mut header = serde_json::Map::new();
            if let Some(id) = app_id {
                header.insert("app_control_id".into(), Value::String(id));
            }
            let ack = {
                let actor = actor_mutex.lock().await;
                let handle = actor
                    .ws
                    .as_ref()
                    .ok_or_else(|| LabError::Scenario("actor has no active WebSocket".into()))?;
                let _ = handle
                    .cmd_tx
                    .send(WsCmd::SendControl {
                        header: Value::Object(header),
                        connect_options: opts,
                    })
                    .await;
                handle
                    .wait_for(Duration::from_secs(3), |ev| {
                        matches!(ev, SignalingEvent::ControlResult(_))
                    })
                    .await?
            };
            let outcome = match ack {
                SignalingEvent::ControlResult(ControlAck::Success {
                    ice_id,
                    client_id,
                    app_control_id,
                    ice_servers,
                    force_relay,
                }) => {
                    let mut actor = actor_mutex.lock().await;
                    actor.last_ice_id = Some(ice_id.clone());
                    actor.last_peer_client_id = Some(client_id.clone());
                    StepOutcome {
                        http_status: None,
                        code: None,
                        ack_status: Some("success".into()),
                        event_name: Some("control_ack".into()),
                        payload: json!({
                            "status": "success",
                            "ice_id": ice_id,
                            "client_id": client_id,
                            "app_control_id": app_control_id,
                            "iceServers": ice_servers,
                            "force_relay": force_relay,
                        }),
                    }
                }
                SignalingEvent::ControlResult(ControlAck::Failure { reason }) => StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: Some("failure".into()),
                    event_name: Some("control_ack".into()),
                    payload: json!({
                        "status": "failure",
                        "reason": reason,
                    }),
                },
                other => {
                    return Err(LabError::Scenario(format!(
                        "unexpected control response: {other:?}"
                    )));
                }
            };
            ("send_control", outcome)
        }
        StepAction::SendSoac {
            ice_id,
            client_id,
            kind,
            sdp,
            candidate,
        } => {
            let ice_id = env.resolve_template(ice_id).await;
            let client_id = match client_id {
                Some(c) => Some(env.resolve_template(c).await),
                None => None,
            };
            let sdp = match sdp {
                Some(s) => Some(env.resolve_template(s).await),
                None => None,
            };
            let candidate = match candidate {
                Some(c) => Some(env.resolve_template(c).await),
                None => None,
            };
            let mut data = json!({
                "type": kind,
                "ice_id": ice_id,
            });
            if let Some(sdp_str) = &sdp {
                data["sdp"] = Value::String(sdp_str.clone());
                if sdp_str.contains("a=ice-pwd:") {
                    for line in sdp_str.lines() {
                        if let Some(pwd) = line.strip_prefix("a=ice-pwd:") {
                            env.record_secret("sdp_ice_pwd", pwd.trim());
                        }
                    }
                }
            }
            if let Some(cand) = candidate {
                data["candidate"] = Value::String(cand);
            }
            let mut body = json!({ "data": data });
            if let Some(cid) = client_id {
                body["client_id"] = Value::String(cid);
            }
            let actor = actor_mutex.lock().await;
            let handle = actor
                .ws
                .as_ref()
                .ok_or_else(|| LabError::Scenario("actor has no active WebSocket".into()))?;
            let _ = handle.cmd_tx.send(WsCmd::SendSoac(body)).await;
            (
                "send_soac",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("soac_sent".into()),
                    payload: json!({ "ice_id": ice_id, "type": kind }),
                },
            )
        }
        StepAction::SendForwardSetting {
            ice_id,
            client_id,
            data,
        } => {
            let ice_id = env.resolve_template(ice_id).await;
            let client_id = match client_id {
                Some(c) => Some(env.resolve_template(c).await),
                None => None,
            };
            let mut resolved_data = env.resolve_json(data).await;
            if let Some(obj) = resolved_data.as_object_mut() {
                obj.entry("ice_id").or_insert_with(|| json!(ice_id));
            }
            let mut body = json!({ "data": resolved_data });
            if let Some(cid) = client_id {
                body["client_id"] = Value::String(cid);
            }
            let actor = actor_mutex.lock().await;
            let handle = actor
                .ws
                .as_ref()
                .ok_or_else(|| LabError::Scenario("actor has no active WebSocket".into()))?;
            let _ = handle.cmd_tx.send(WsCmd::SendForwardSetting(body)).await;
            (
                "send_forward_setting",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("forward_setting_sent".into()),
                    payload: json!({ "ice_id": ice_id }),
                },
            )
        }
        StepAction::WaitEvent { event, timeout_ms } => {
            let target_name = event.clone();
            let ev = {
                let actor = actor_mutex.lock().await;
                let handle = actor
                    .ws
                    .as_ref()
                    .ok_or_else(|| LabError::Scenario("actor has no active WebSocket".into()))?;
                handle
                    .wait_for(Duration::from_millis(*timeout_ms), |ev| match ev {
                        SignalingEvent::Signal(sig) => sig.name() == target_name,
                        SignalingEvent::Closed => target_name == "closed",
                        _ => false,
                    })
                    .await?
            };
            let (ev_name, payload, new_ice, new_peer) = match ev {
                SignalingEvent::Signal(sig) => match sig {
                    SignalEvent::BeControlled(bc) => {
                        let p = json!({
                            "ice_id": bc.ice_id,
                            "client_id": bc.client_id,
                            "app_control_id": bc.app_control_id,
                            "force_relay": bc.force_relay,
                            "iceServers": bc.ice_servers,
                            "connect_options_len": bc.connect_options.len(),
                        });
                        (
                            "be-controlled".to_owned(),
                            p,
                            Some(bc.ice_id),
                            Some(bc.client_id),
                        )
                    }
                    SignalEvent::Soac(soac) => {
                        let p = serde_json::to_value(&soac)?;
                        ("soac".to_owned(), p, None, None)
                    }
                    SignalEvent::ForwardSetting(fs) => {
                        let p = serde_json::to_value(&fs)?;
                        ("forward_setting".to_owned(), p, None, None)
                    }
                    SignalEvent::Released(rel) => {
                        let p = serde_json::to_value(&rel)?;
                        ("released".to_owned(), p, None, None)
                    }
                    SignalEvent::Left(left) => {
                        let mut p = serde_json::to_value(&left)?;
                        if let Some(obj) = p.as_object_mut() {
                            obj.insert("is_room_kick".into(), json!(left.is_room_kick()));
                        }
                        ("left".to_owned(), p, None, None)
                    }
                    SignalEvent::BmsgPush(push) => {
                        let p = serde_json::to_value(&push)?;
                        ("bmsg_push".to_owned(), p, None, None)
                    }
                    other => (other.name().to_owned(), json!({}), None, None),
                },
                SignalingEvent::Closed => ("closed".to_owned(), json!({}), None, None),
                _ => ("other".to_owned(), json!({}), None, None),
            };
            if new_ice.is_some() || new_peer.is_some() {
                let mut actor = actor_mutex.lock().await;
                if let Some(ice) = new_ice {
                    actor.last_ice_id = Some(ice);
                }
                if let Some(peer) = new_peer {
                    actor.last_peer_client_id = Some(peer);
                }
            }
            (
                "wait_event",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some(ev_name),
                    payload,
                },
            )
        }
        StepAction::PollDeviceStatus {
            target,
            expected_status,
            timeout_ms,
        } => {
            let target_id = resolve_target_device(env, target).await?;
            let deadline = tokio::time::Instant::now() + Duration::from_millis(*timeout_ms);
            let matched_payload = loop {
                let resp = {
                    let actor = actor_mutex.lock().await;
                    let tok = actor.user_token.clone().unwrap_or_default();
                    actor
                        .http
                        .device_list(&tok, actor.device_id.as_deref())
                        .await?
                };
                let val = serde_json::to_value(&resp)?;
                if let Some(list) = val.get("my_binded_devices").and_then(Value::as_array)
                    && let Some(found) = list.iter().find(|d| {
                        d.get("device_id").and_then(Value::as_str) == Some(target_id.as_str())
                    })
                    && found.get("status").and_then(Value::as_str) == Some(expected_status.as_str())
                {
                    break val;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(LabError::Scenario(format!(
                        "device `{target_id}` did not reach status `{expected_status}` within {timeout_ms}ms; last list: {val}"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            ("poll_device_status", StepOutcome::http_ok(matched_payload))
        }
        StepAction::RawHttp {
            method,
            path,
            body,
            raw_body,
            token,
            device_id,
            sign: should_sign,
            headers,
        } => {
            let resolved_path = env.resolve_template(path).await;
            let resolved_body = match body {
                Some(b) => Some(env.resolve_json(b).await),
                None => None,
            };
            let resolved_raw_body = match raw_body {
                Some(r) => Some(env.resolve_template(r).await),
                None => None,
            };
            let resolved_token = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let resolved_device = match device_id {
                Some(d) => Some(env.resolve_template(d).await),
                None => None,
            };
            let actor = actor_mutex.lock().await;
            let out = send_raw_http(
                &actor.raw_client,
                &env.base_url,
                env.sign_key.as_ref(),
                method,
                &resolved_path,
                resolved_body,
                resolved_raw_body.as_deref(),
                resolved_token.as_deref(),
                resolved_device.as_deref(),
                *should_sign,
                headers,
                &actor.recorder,
            )
            .await?;
            ("raw_http", out)
        }
        StepAction::SleepMs { ms } => {
            tokio::time::sleep(Duration::from_millis(*ms)).await;
            (
                "sleep_ms",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: None,
                    payload: json!({ "slept_ms": ms }),
                },
            )
        }
        StepAction::HeadlessRoundtrip { expected_frames } => {
            let input_script = vec![
                InputMessage::Keyboard {
                    key: 65,
                    pressed: true,
                },
                InputMessage::MouseAbsolute { x: 0.5, y: 0.5 },
            ];
            let mut platform = HeadlessPlatform::new()
                .with_video_source(AnnexBFileVideoSource::from_bytes(
                    &synthetic_annex_b_sample(),
                )?)
                .with_input_source(ScriptedInputSource::new(input_script.clone()));

            // Pump video frames from source to sink, and input events from source to sink.
            let mut drained_frames = Vec::new();
            if let Some(vs) = platform.video_source() {
                while let Some(frame) = vs.next_frame()? {
                    drained_frames.push(frame);
                }
            }
            if let Some(sink) = platform.video_sink() {
                for frame in &drained_frames {
                    sink.push_frame(frame)?;
                }
            }
            let mut drained_inputs = Vec::new();
            if let Some(is) = platform.input_source() {
                while let Some(ev) = is.next_event()? {
                    drained_inputs.push(ev);
                }
            }
            if let Some(sink) = platform.input_sink() {
                for ev in &drained_inputs {
                    sink.inject(ev)?;
                }
            }
            if platform.video_sink.frames != *expected_frames {
                return Err(LabError::Scenario(format!(
                    "expected {expected_frames} headless video frames, counted {}",
                    platform.video_sink.frames
                )));
            }
            if platform.input_sink.recorded() != input_script {
                return Err(LabError::Scenario(
                    "headless input sink did not record expected events".into(),
                ));
            }
            (
                "headless_roundtrip",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("headless_ok".into()),
                    payload: json!({
                        "frames": platform.video_sink.frames,
                        "keyframes": platform.video_sink.keyframes,
                        "inputs": platform.input_sink.len(),
                    }),
                },
            )
        }
        StepAction::SessionHostStart {
            force_relay,
            reject_reason,
            loop_video,
        } => {
            let (ep, prev_host) = {
                let mut actor = actor_mutex.lock().await;
                let ep = build_actor_endpoint(&actor, env, None)?;
                (ep, actor.host_session.take())
            };
            if let Some(prev) = prev_host {
                prev.close().await;
            }
            let input_sink = RecordingInputSink::new();
            let clipboard = MemoryClipboard::new();
            let video_src = AnnexBFileVideoSource::from_bytes(&synthetic_annex_b_sample())?
                .with_loop(*loop_video);
            let mut platform = HeadlessPlatform::new().with_video_source(video_src);
            platform.input_sink = input_sink.clone();
            platform.clipboard = clipboard.clone();

            let mut cfg = HostConfig {
                force_relay: *force_relay,
                ..Default::default()
            };
            if let Some(reason) = reject_reason {
                let reason = reason.clone();
                cfg.authorizer = Some(Arc::new(move |_| AuthorizationDecision::Reject {
                    reason: reason.clone(),
                }));
            }

            let session = HostSession::start(&ep, Box::new(platform), cfg)
                .await
                .map_err(|e| LabError::Scenario(format!("host session start failed: {e}")))?;
            let dev_id = session.device_id().to_owned();
            {
                let mut actor = actor_mutex.lock().await;
                actor.device_id = Some(dev_id.clone());
                actor.input_sink = Some(input_sink);
                actor.clipboard_mirror = Some(clipboard);
                actor.host_session = Some(session);
            }
            (
                "session_host_start",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("host_started".into()),
                    payload: json!({
                        "device_id": dev_id,
                        "force_relay": force_relay,
                    }),
                },
            )
        }
        StepAction::SessionViewerConnect {
            target,
            token,
            force_relay,
            force_join,
            timeout_ms,
        } => {
            let target_id = resolve_target_device(env, target).await?;
            let explicit_tok = match token {
                Some(t) => Some(env.resolve_template(t).await),
                None => None,
            };
            let (ep, has_token, prev_viewer) = {
                let mut actor = actor_mutex.lock().await;
                let has_tok = explicit_tok.is_some() || actor.user_token.is_some();
                let ep = build_actor_endpoint(&actor, env, explicit_tok.as_deref())?;
                (ep, has_tok, actor.viewer_session.take())
            };
            if let Some(prev) = prev_viewer {
                prev.close().await;
            }
            if !has_token {
                let login = ep
                    .login()
                    .await
                    .map_err(|e| LabError::Scenario(format!("viewer auto-login failed: {e}")))?;
                env.record_secret("user_token", &login.token);
                actor_mutex.lock().await.user_token = Some(login.token);
            }
            let clipboard = MemoryClipboard::new();
            let mut platform = HeadlessPlatform::new();
            platform.clipboard = clipboard.clone();

            let cfg = ViewerConfig {
                force_relay: *force_relay,
                force_join: *force_join,
                connect_timeout: Duration::from_millis(*timeout_ms),
                ..Default::default()
            };

            let session = ViewerSession::connect(&ep, &target_id, Box::new(platform), cfg)
                .await
                .map_err(|e| LabError::Scenario(format!("viewer connect failed: {e}")))?;
            let stats = session
                .wait_ready(Duration::from_millis(*timeout_ms))
                .await
                .map_err(|e| LabError::Scenario(format!("viewer wait_ready failed: {e}")))?;
            let ice_id = session.ice_id().to_owned();
            {
                let mut actor = actor_mutex.lock().await;
                actor.last_ice_id = Some(ice_id.clone());
                actor.clipboard_mirror = Some(clipboard);
                actor.viewer_session = Some(session);
            }
            (
                "session_viewer_connect",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: Some("success".into()),
                    event_name: Some("peer_ready".into()),
                    payload: json!({
                        "ice_id": ice_id,
                        "connected": stats.connected,
                        "peer_ready": stats.peer_ready,
                        "is_relayed": stats.is_relayed,
                    }),
                },
            )
        }
        StepAction::SessionSendInput {
            kind,
            key,
            button,
            x,
            y,
            dx,
            dy,
            text,
        } => {
            let msg = build_input_message(kind, *key, *button, *x, *y, *dx, *dy, text.as_deref())?;
            {
                let mut actor = actor_mutex.lock().await;
                let sess = actor.viewer_session.as_ref().ok_or_else(|| {
                    LabError::Scenario("actor has no active viewer_session".into())
                })?;
                sess.send_input(msg.clone())
                    .await
                    .map_err(|e| LabError::Scenario(format!("send_input failed: {e}")))?;
                actor.sent_inputs.push(msg);
            }
            (
                "session_send_input",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("input_sent".into()),
                    payload: json!({ "kind": kind }),
                },
            )
        }
        StepAction::SessionSetClipboard { text } => {
            let resolved = env.resolve_template(text).await;
            let actor = actor_mutex.lock().await;
            if let Some(sess) = &actor.host_session {
                sess.set_clipboard(resolved.clone())
                    .await
                    .map_err(|e| LabError::Scenario(format!("host set_clipboard failed: {e}")))?;
            } else if let Some(sess) = &actor.viewer_session {
                sess.send_clipboard(resolved.clone()).await.map_err(|e| {
                    LabError::Scenario(format!("viewer send_clipboard failed: {e}"))
                })?;
            } else {
                return Err(LabError::Scenario(
                    "actor has no active host_session or viewer_session".into(),
                ));
            }
            (
                "session_set_clipboard",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("clipboard_sent".into()),
                    payload: json!({ "len": resolved.len() }),
                },
            )
        }
        StepAction::SessionWait {
            min_frames,
            min_inputs,
            expected_clipboard,
            require_relayed,
            require_p2p,
            timeout_ms,
        } => {
            let expected_clip = match expected_clipboard {
                Some(s) => Some(env.resolve_template(s).await),
                None => None,
            };
            let deadline = tokio::time::Instant::now() + Duration::from_millis(*timeout_ms);
            let outcome_payload = loop {
                let mut ok = true;
                let is_relayed;
                let connected;
                let peer_ready;
                let mut frames_ok = min_frames.is_none();
                let mut inputs_ok = min_inputs.is_none();
                let mut clip_ok = expected_clip.is_none();
                let mut diag = String::new();

                {
                    let actor = actor_mutex.lock().await;
                    if let Some(vsess) = &actor.viewer_session {
                        let st = vsess.stats();
                        connected = st.connected;
                        peer_ready = st.peer_ready;
                        is_relayed = st.is_relayed;
                        if let Some(min_f) = min_frames {
                            frames_ok = st.video_frames_received >= *min_f;
                            if !frames_ok {
                                ok = false;
                                diag = format!("frames={} < {min_f}", st.video_frames_received);
                            }
                        }
                        if let Some(exp) = &expected_clip {
                            let cur = actor
                                .clipboard_mirror
                                .as_ref()
                                .and_then(|c| c.text())
                                .or(st.last_clipboard);
                            clip_ok = cur.as_deref() == Some(exp.as_str());
                            if !clip_ok {
                                ok = false;
                                diag = format!("viewer clipboard={cur:?} != {exp:?}");
                            }
                        }
                    } else if let Some(hsess) = &actor.host_session {
                        let st = hsess.stats();
                        connected = st.connected;
                        peer_ready = st.peer_ready;
                        is_relayed = st.is_relayed;
                        if let Some(min_i) = min_inputs {
                            let count = actor
                                .input_sink
                                .as_ref()
                                .map_or(st.inputs_injected as usize, RecordingInputSink::len);
                            inputs_ok = count as u64 >= *min_i;
                            if !inputs_ok {
                                ok = false;
                                diag = format!("inputs={count} < {min_i}");
                            }
                        }
                        if let Some(exp) = &expected_clip {
                            let cur = actor.clipboard_mirror.as_ref().and_then(|c| c.text());
                            clip_ok = cur.as_deref() == Some(exp.as_str());
                            if !clip_ok {
                                ok = false;
                                diag = format!("host clipboard={cur:?} != {exp:?}");
                            }
                        }
                    } else {
                        return Err(LabError::Scenario(
                            "session_wait requires an active host_session or viewer_session".into(),
                        ));
                    }

                    if *require_relayed == Some(true) && !is_relayed {
                        ok = false;
                        diag = "expected relayed ICE transport".into();
                    }
                    if *require_p2p == Some(true) && is_relayed {
                        ok = false;
                        diag = "expected direct P2P ICE transport, got relayed".into();
                    }
                }

                if ok {
                    break json!({
                        "connected": connected,
                        "peer_ready": peer_ready,
                        "is_relayed": is_relayed,
                        "min_frames_met": frames_ok,
                        "min_inputs_met": inputs_ok,
                        "clipboard_matched": clip_ok,
                    });
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(LabError::Scenario(format!(
                        "session_wait timed out after {timeout_ms}ms ({diag})"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            };

            // Also verify exact input roundtrip if viewer sent inputs and this actor is host.
            if min_inputs.is_some() {
                let mut all_sent: Vec<InputMessage> = Vec::new();
                for (name, other_mutex) in &env.actors {
                    if name != &step.actor {
                        all_sent.extend(other_mutex.lock().await.sent_inputs.clone());
                    }
                }
                if !all_sent.is_empty()
                    && let Some(sink) = actor_mutex.lock().await.input_sink.as_ref()
                {
                    let recorded = sink.recorded();
                    for msg in &all_sent {
                        if !recorded.contains(msg) {
                            return Err(LabError::Scenario(format!(
                                "host input sink missed expected event: {msg:?}"
                            )));
                        }
                    }
                }
            }

            (
                "session_wait",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("session_ok".into()),
                    payload: outcome_payload,
                },
            )
        }
        StepAction::SessionClose => {
            let (viewer_sess, host_sess) = {
                let mut actor = actor_mutex.lock().await;
                (actor.viewer_session.take(), actor.host_session.take())
            };
            if let Some(sess) = viewer_sess {
                sess.close().await;
            }
            if let Some(sess) = host_sess {
                sess.close().await;
            }
            (
                "session_close",
                StepOutcome {
                    http_status: None,
                    code: None,
                    ack_status: None,
                    event_name: Some("session_closed".into()),
                    payload: json!({ "closed": true }),
                },
            )
        }
    };

    if !step.save.is_empty() {
        let mut vars = env.vars.lock().await;
        for (var_name, path) in &step.save {
            let val = json_path(&outcome.payload, path).ok_or_else(|| {
                LabError::Scenario(format!(
                    "save `{var_name}` path `{path}` not found in payload: {}",
                    redact_value(&outcome.payload)
                ))
            })?;
            let as_str = match val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            vars.insert(var_name.clone(), as_str);
        }
    }

    if let Some(expect) = &step.expect {
        verify_expect(env, expect, &outcome, step_idx, &step.actor, action_label).await?;
    }

    env.observed.lock().await.push((
        primary_idx,
        secondary_idx,
        ObservedStep {
            index: step_idx,
            id: step.id.clone(),
            actor: step.actor.clone(),
            action: action_label.to_owned(),
            http_status: outcome.http_status,
            code: outcome.code,
            ack_status: outcome.ack_status,
            event_name: outcome.event_name,
            payload: outcome.payload,
        },
    ));

    Ok(())
}

fn build_actor_endpoint(
    actor: &ActorState,
    env: &SharedEnv,
    explicit_token: Option<&str>,
) -> Result<Endpoint, LabError> {
    let mut builder = Endpoint::builder(actor.profile)
        .base_url(&env.base_url)
        .client_id(&actor.client_id)
        .device_name(&actor.device_name)
        .http_client(actor.raw_client.clone())
        .recorder(Arc::clone(&actor.recorder))
        .auto_reconnect(false);
    if let Some(key) = &env.sign_key {
        builder = builder.sign_key(key.clone());
    }
    if let Some(dev_id) = &actor.device_id {
        builder = builder.device_id(dev_id.clone());
    }
    if let Some(tok) = explicit_token.or(actor.user_token.as_deref()) {
        builder = builder.user_token(tok.to_owned());
    }
    builder
        .build()
        .map_err(|e| LabError::Scenario(format!("failed to build endpoint: {e}")))
}

#[allow(clippy::too_many_arguments)]
fn build_input_message(
    kind: &str,
    key: Option<i32>,
    button: Option<i32>,
    x: Option<f64>,
    y: Option<f64>,
    dx: Option<i32>,
    dy: Option<i32>,
    text: Option<&str>,
) -> Result<InputMessage, LabError> {
    match kind {
        "kbd_press" => Ok(InputMessage::Keyboard {
            key: key.ok_or_else(|| LabError::Scenario("kbd_press requires key".into()))?,
            pressed: true,
        }),
        "kbd_release" => Ok(InputMessage::Keyboard {
            key: key.ok_or_else(|| LabError::Scenario("kbd_release requires key".into()))?,
            pressed: false,
        }),
        "kbd_click" => Ok(InputMessage::KeyboardClick {
            key: key.ok_or_else(|| LabError::Scenario("kbd_click requires key".into()))?,
        }),
        "mouse_press" => Ok(InputMessage::MouseButton {
            button: button
                .ok_or_else(|| LabError::Scenario("mouse_press requires button".into()))?,
            pressed: true,
        }),
        "mouse_release" => Ok(InputMessage::MouseButton {
            button: button
                .ok_or_else(|| LabError::Scenario("mouse_release requires button".into()))?,
            pressed: false,
        }),
        "mouse_click" => Ok(InputMessage::MouseClick {
            button: button
                .ok_or_else(|| LabError::Scenario("mouse_click requires button".into()))?,
        }),
        "mouse_move_absolute" => Ok(InputMessage::MouseAbsolute {
            x: x.ok_or_else(|| LabError::Scenario("mouse_move_absolute requires x".into()))?,
            y: y.ok_or_else(|| LabError::Scenario("mouse_move_absolute requires y".into()))?,
        }),
        "mouse_move_relative" => Ok(InputMessage::MouseRelative {
            dx: dx.ok_or_else(|| LabError::Scenario("mouse_move_relative requires dx".into()))?,
            dy: dy.ok_or_else(|| LabError::Scenario("mouse_move_relative requires dy".into()))?,
        }),
        "mouse_scroll" => Ok(InputMessage::MouseScroll {
            dx: dx.unwrap_or(0),
            dy: dy.unwrap_or(0),
        }),
        "text_input" => Ok(InputMessage::TextInput {
            text: text
                .ok_or_else(|| LabError::Scenario("text_input requires text".into()))?
                .to_owned(),
        }),
        other => Err(LabError::Scenario(format!("unknown input kind `{other}`"))),
    }
}

async fn resolve_target_device(env: &SharedEnv, target: &str) -> Result<String, LabError> {
    if target.contains("${") {
        return Ok(env.resolve_template(target).await);
    }
    if let Some(actor_mutex) = env.actors.get(target) {
        let actor = actor_mutex.lock().await;
        if let Some(id) = &actor.device_id {
            return Ok(id.clone());
        }
    }
    Ok(target.to_owned())
}

struct StepOutcome {
    http_status: Option<u16>,
    code: Option<i64>,
    ack_status: Option<String>,
    event_name: Option<String>,
    payload: Value,
}

impl StepOutcome {
    fn http_ok(payload: Value) -> Self {
        Self {
            http_status: Some(200),
            code: Some(0),
            ack_status: None,
            event_name: None,
            payload,
        }
    }

    fn from_http_err(err: HttpError) -> Result<Self, LabError> {
        match err {
            HttpError::Api(api_err) => {
                let (http_status, code) = match api_err {
                    ApiError::TokenExpired => (422, 1120),
                    ApiError::QrCodeExpired => (400, 1190),
                    ApiError::Rejected { http_status, code } => (http_status, code),
                    ApiError::Malformed { http_status } => (http_status, -1),
                    other => return Err(LabError::Http(HttpError::Api(other))),
                };
                Ok(Self {
                    http_status: Some(http_status),
                    code: Some(code),
                    ack_status: None,
                    event_name: None,
                    payload: json!({ "error_code": code, "http_status": http_status }),
                })
            }
            other => Err(LabError::Http(other)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_raw_http(
    client: &reqwest::Client,
    base_url: &str,
    sign_key: Option<&SignKey>,
    method: &str,
    path: &str,
    body: Option<Value>,
    raw_body: Option<&str>,
    token: Option<&str>,
    device_id: Option<&str>,
    should_sign: bool,
    extra_headers: &BTreeMap<String, String>,
    recorder: &TraceRecorder,
) -> Result<StepOutcome, LabError> {
    let base = Url::parse(base_url).map_err(|e| LabError::Target(e.to_string()))?;
    let url = base
        .join(path)
        .map_err(|e| LabError::Target(e.to_string()))?;
    let signed_path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    };
    let req_method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|e| LabError::Scenario(e.to_string()))?;
    let bytes = if let Some(raw) = raw_body {
        raw.as_bytes().to_vec()
    } else if let Some(val) = body {
        serde_json::to_vec(&val)?
    } else {
        Vec::new()
    };

    let mut hdrs = reqwest::header::HeaderMap::new();
    let mut param_pairs: Vec<(String, String)> = Vec::new();
    if !bytes.is_empty() {
        hdrs.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
    }
    if let Some(tok) = token {
        let val = reqwest::header::HeaderValue::from_str(&format!("Bearer {tok}"))
            .map_err(|_| LabError::Scenario("invalid bearer token header".into()))?;
        hdrs.insert(reqwest::header::AUTHORIZATION, val);
    }
    if let Some(dev) = device_id {
        let val = reqwest::header::HeaderValue::from_str(dev)
            .map_err(|_| LabError::Scenario("invalid device header".into()))?;
        hdrs.insert(
            reqwest::header::HeaderName::from_static("x-param-device-id"),
            val,
        );
        param_pairs.push(("x-param-device-id".into(), dev.to_owned()));
    }
    for (k, v) in extra_headers {
        let hn = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| LabError::Scenario(format!("invalid header `{k}`")))?;
        let hv = reqwest::header::HeaderValue::from_str(v)
            .map_err(|_| LabError::Scenario(format!("invalid header value for `{k}`")))?;
        if k.to_ascii_lowercase().starts_with(PARAM_PREFIX) {
            param_pairs.push((k.to_ascii_lowercase(), v.clone()));
        }
        hdrs.insert(hn, hv);
    }
    if should_sign && let Some(key) = sign_key {
        let sig = sign(
            key,
            req_method.as_str(),
            &signed_path,
            param_pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            &bytes,
        );
        hdrs.insert(
            reqwest::header::HeaderName::from_static(SIGN_HEADER),
            reqwest::header::HeaderValue::from_str(&sig).expect("hex sign is valid header"),
        );
    }

    recorder.record_http_request(req_method.as_str(), &signed_path, &bytes);
    let resp = client
        .request(req_method.clone(), url)
        .headers(hdrs)
        .body(bytes)
        .send()
        .await
        .map_err(|e| LabError::Target(e.without_url().to_string()))?;
    let status = resp.status().as_u16();
    let resp_bytes = resp
        .bytes()
        .await
        .map_err(|e| LabError::Target(e.to_string()))?
        .to_vec();
    recorder.record_http_response(req_method.as_str(), &signed_path, status, &resp_bytes);

    let json_body: Value = serde_json::from_slice(&resp_bytes).unwrap_or(Value::Null);
    let code = json_body.get("code").and_then(Value::as_i64);
    let payload = json_body.get("data").cloned().unwrap_or(json_body);

    Ok(StepOutcome {
        http_status: Some(status),
        code,
        ack_status: None,
        event_name: None,
        payload,
    })
}

async fn spawn_ws_handle(
    ws_url: String,
    room_token: String,
    profile: &'static Profile,
    recorder: Arc<TraceRecorder>,
) -> Result<WsHandle, LabError> {
    let mut socket =
        SignalingSocket::connect(&ws_url, &room_token, profile, None, Some(recorder)).await?;
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<WsCmd>(32);
    let (ev_tx, mut ev_rx) = mpsc::channel::<SignalingEvent>(64);
    let events = Arc::new(Mutex::new(VecDeque::new()));
    let notify = Arc::new(Notify::new());

    let events_writer = Arc::clone(&events);
    let notify_writer = Arc::clone(&notify);
    tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            events_writer.lock().await.push_back(ev);
            notify_writer.notify_waiters();
        }
        events_writer.lock().await.push_back(SignalingEvent::Closed);
        notify_writer.notify_waiters();
    });

    let task = tokio::spawn(async move {
        let anchor = tokio::time::Instant::now();
        let mut machine = SignalingMachine::with_signal_profile(
            rand::rngs::StdRng::from_os_rng(),
            &profile.signal,
        );
        machine.connect(Timestamp::ZERO);

        let _ = drive_machine_with(
            &mut machine,
            &mut socket,
            anchor,
            &mut cmd_rx,
            &ev_tx,
            |m, _now, cmd| match cmd {
                WsCmd::SendRoomInfo => {
                    m.send_room_info();
                    ControlFlow::Continue(())
                }
                WsCmd::SendRefreshReconnectKey => {
                    m.send_refresh_reconnect_key();
                    ControlFlow::Continue(())
                }
                WsCmd::SendControl {
                    header,
                    connect_options,
                } => {
                    m.send_control(header, connect_options);
                    ControlFlow::Continue(())
                }
                WsCmd::SendSoac(payload) => {
                    m.send_soac(payload);
                    ControlFlow::Continue(())
                }
                WsCmd::SendForwardSetting(payload) => {
                    m.send_forward_setting(payload);
                    ControlFlow::Continue(())
                }
                WsCmd::Close => {
                    m.disconnect();
                    ControlFlow::Break(())
                }
            },
            |_m, _now, _ev| {},
        )
        .await;
    });

    Ok(WsHandle {
        cmd_tx,
        events,
        notify,
        task,
    })
}

async fn verify_expect(
    env: &SharedEnv,
    expect: &Expect,
    outcome: &StepOutcome,
    step_idx: usize,
    actor: &str,
    action: &str,
) -> Result<(), LabError> {
    if let Some(expected_ok) = expect.ok {
        let is_ok = outcome.code.is_none_or(|c| c == 0)
            && outcome.ack_status.as_deref().is_none_or(|s| s != "failure");
        if is_ok != expected_ok {
            return Err(LabError::Scenario(format!(
                "step {step_idx} ({actor}.{action}): expected ok={expected_ok}, got code={:?}, ack={:?}",
                outcome.code, outcome.ack_status
            )));
        }
    }
    if let Some(expected_status) = expect.http_status
        && outcome.http_status != Some(expected_status)
    {
        return Err(LabError::Scenario(format!(
            "step {step_idx} ({actor}.{action}): expected http_status={expected_status}, got {:?} (code={:?})",
            outcome.http_status, outcome.code
        )));
    }
    if let Some(expected_code) = expect.code
        && outcome.code != Some(expected_code)
    {
        return Err(LabError::Scenario(format!(
            "step {step_idx} ({actor}.{action}): expected code={expected_code}, got {:?} (http_status={:?})",
            outcome.code, outcome.http_status
        )));
    }
    if let Some(expected_ack) = &expect.ack_status
        && outcome.ack_status.as_deref() != Some(expected_ack.as_str())
    {
        return Err(LabError::Scenario(format!(
            "step {step_idx} ({actor}.{action}): expected ack_status={expected_ack}, got {:?}",
            outcome.ack_status
        )));
    }
    if let Some(expected_event) = &expect.event_name
        && outcome.event_name.as_deref() != Some(expected_event.as_str())
    {
        return Err(LabError::Scenario(format!(
            "step {step_idx} ({actor}.{action}): expected event_name={expected_event}, got {:?}",
            outcome.event_name
        )));
    }
    for (path, expected_raw) in &expect.json_eq {
        let expected = env.resolve_json(expected_raw).await;
        let actual = json_path(&outcome.payload, path);
        if actual != Some(&expected) {
            return Err(LabError::Scenario(format!(
                "step {step_idx} ({actor}.{action}): path `{path}` expected {}, got {:?}",
                redact_value(&expected),
                actual.map(redact_value)
            )));
        }
    }
    for path in &expect.json_exists {
        let actual = json_path(&outcome.payload, path);
        let non_empty = match actual {
            None | Some(Value::Null) => false,
            Some(Value::String(s)) => !s.is_empty(),
            Some(_) => true,
        };
        if !non_empty {
            return Err(LabError::Scenario(format!(
                "step {step_idx} ({actor}.{action}): expected non-empty path `{path}` in {}",
                redact_value(&outcome.payload)
            )));
        }
    }
    for (path, subset_raw) in &expect.json_contains {
        let subset = env.resolve_json(subset_raw).await;
        let Some(Value::Array(items)) = json_path(&outcome.payload, path) else {
            return Err(LabError::Scenario(format!(
                "step {step_idx} ({actor}.{action}): expected array at `{path}`"
            )));
        };
        if !items.iter().any(|item| json_matches_subset(item, &subset)) {
            return Err(LabError::Scenario(format!(
                "step {step_idx} ({actor}.{action}): array at `{path}` has no item matching {}",
                redact_value(&subset)
            )));
        }
    }
    Ok(())
}

fn json_matches_subset(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(a_map), Value::Object(e_map)) => e_map
            .iter()
            .all(|(k, ev)| a_map.get(k).is_some_and(|av| json_matches_subset(av, ev))),
        _ => actual == expected,
    }
}

fn json_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path.split('.') {
        if let Ok(idx) = seg.parse::<usize>()
            && let Some(arr) = cur.as_array()
        {
            cur = arr.get(idx)?;
            continue;
        }
        cur = cur.get(seg)?;
    }
    Some(cur)
}
