//! Spawns and supervises `remora` cloud binaries in standalone or role-split configurations.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message as AxumWsMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use remora_proto::control::SignKey;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as TungWsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::error::LabError;
use crate::target::turn::TestTurnServer;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Locates the `remora` binary from explicit CLI flag, `REMORA_BIN` env, cargo target dir, or `PATH`.
pub fn find_remora_bin(explicit: Option<&Path>) -> Result<PathBuf, LabError> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        return Err(LabError::Target(format!(
            "remora binary not found at {}",
            p.display()
        )));
    }
    if let Ok(env_path) = std::env::var("REMORA_BIN") {
        let p = PathBuf::from(env_path);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for candidate in [dir.join("remora"), dir.join("../remora")] {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join("remora");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(LabError::Target(
        "could not find `remora` binary; pass --remora-bin or set REMORA_BIN".into(),
    ))
}

struct TempWorkspace {
    dir: PathBuf,
}

impl TempWorkspace {
    fn new(prefix: &str) -> Result<Self, LabError> {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Running `remora` instance(s) with temporary database, random ports, and captured logs.
pub struct RemoraInstance {
    base_url: String,
    sign_key: Option<SignKey>,
    children: Vec<Child>,
    tasks: Vec<JoinHandle<()>>,
    logs: Arc<Mutex<String>>,
    _turn: Option<TestTurnServer>,
    _workspace: TempWorkspace,
}

impl Drop for RemoraInstance {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for child in &mut self.children {
            let _ = child.start_kill();
        }
    }
}

impl RemoraInstance {
    /// Spawns a single `remora` process running all roles (`api,gateway,room,relay`).
    pub async fn spawn_standalone(
        remora_bin: &Path,
        sign_key: Option<&str>,
    ) -> Result<Self, LabError> {
        let workspace = TempWorkspace::new("remora-lab-standalone")?;
        let port = pick_free_port()?;
        let base_url = format!("http://127.0.0.1:{port}");
        let db_path = workspace.path().join("remora.db");
        let turn_secret_path = workspace.path().join("turn.secret");
        std::fs::write(&turn_secret_path, "lab-turn-secret\n")?;
        let (turn_url, turn_guard) = match std::env::var("REMORA_TURN_URL") {
            Ok(u) if !u.is_empty() => (u, None),
            _ => {
                let srv = TestTurnServer::spawn("lab-turn-secret").await?;
                (srv.turn_url(), Some(srv))
            }
        };

        let mut toml = format!(
            "roles = [\"api\", \"gateway\", \"room\", \"relay\"]\n\
             listen = \"127.0.0.1:{port}\"\n\
             public_url = \"{base_url}\"\n\
             database = \"{}\"\n\
             log = \"info\"\n",
            db_path.display()
        );

        let parsed_sign_key = if let Some(key) = sign_key {
            let sign_path = workspace.path().join("sign.key");
            std::fs::write(&sign_path, format!("{key}\n"))?;
            toml.push_str(&format!("sign_key_file = \"{}\"\n", sign_path.display()));
            Some(SignKey::new(key))
        } else {
            None
        };

        toml.push_str(&format!(
            "\n[login]\n\
             auto_confirm = true\n\n\
             [ice]\n\
             servers = [\"{turn_url}\"]\n\
             turn_secret_file = \"{}\"\n",
            turn_secret_path.display()
        ));

        let cfg_path = workspace.path().join("remora.toml");
        std::fs::write(&cfg_path, toml)?;

        let logs = Arc::new(Mutex::new(String::new()));
        let mut tasks = Vec::new();
        let mut child = spawn_remora_process(remora_bin, &cfg_path, &[], &logs, &mut tasks)?;

        wait_http_ready(&base_url, &mut child, &logs).await?;

        Ok(Self {
            base_url,
            sign_key: parsed_sign_key,
            children: vec![child],
            tasks,
            logs,
            _turn: turn_guard,
            _workspace: workspace,
        })
    }

    /// Spawns two role-split `remora` processes (`api,relay` and `gateway,room`) connected via a
    /// Unix-socket bus and fronted by a local reverse proxy on a random port.
    pub async fn spawn_split(remora_bin: &Path, sign_key: Option<&str>) -> Result<Self, LabError> {
        let workspace = TempWorkspace::new("remora-lab-split")?;
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await?;
        let proxy_addr = proxy_listener.local_addr()?;
        let base_url = format!("http://{proxy_addr}");

        let api_port = pick_free_port()?;
        let gw_port = pick_free_port()?;
        let db_path = workspace.path().join("remora.db");
        let bus_sock = workspace.path().join("bus.sock");
        let turn_secret_path = workspace.path().join("turn.secret");
        std::fs::write(&turn_secret_path, "lab-turn-secret\n")?;
        let (turn_url, turn_guard) = match std::env::var("REMORA_TURN_URL") {
            Ok(u) if !u.is_empty() => (u, None),
            _ => {
                let srv = TestTurnServer::spawn("lab-turn-secret").await?;
                (srv.turn_url(), Some(srv))
            }
        };

        let mut api_toml = format!(
            "roles = [\"api\", \"relay\"]\n\
             listen = \"127.0.0.1:{api_port}\"\n\
             public_url = \"{base_url}\"\n\
             database = \"{}\"\n\
             log = \"info\"\n",
            db_path.display()
        );

        let parsed_sign_key = if let Some(key) = sign_key {
            let sign_path = workspace.path().join("sign.key");
            std::fs::write(&sign_path, format!("{key}\n"))?;
            api_toml.push_str(&format!("sign_key_file = \"{}\"\n", sign_path.display()));
            Some(SignKey::new(key))
        } else {
            None
        };

        api_toml.push_str(&format!(
            "\n[login]\n\
             auto_confirm = true\n\n\
             [ice]\n\
             servers = [\"{turn_url}\"]\n\
             turn_secret_file = \"{}\"\n",
            turn_secret_path.display()
        ));

        let gw_toml = format!(
            "roles = [\"gateway\", \"room\"]\n\
             listen = \"127.0.0.1:{gw_port}\"\n\
             public_url = \"{base_url}\"\n\
             database = \"{}\"\n\
             log = \"info\"\n",
            db_path.display()
        );

        let api_cfg = workspace.path().join("api.toml");
        let gw_cfg = workspace.path().join("gw.toml");
        std::fs::write(&api_cfg, api_toml)?;
        std::fs::write(&gw_cfg, gw_toml)?;

        let logs = Arc::new(Mutex::new(String::new()));
        let mut tasks = Vec::new();
        let bus_str = bus_sock.to_string_lossy().into_owned();

        let mut api_child = spawn_remora_process(
            remora_bin,
            &api_cfg,
            &["--bus-listen", &bus_str],
            &logs,
            &mut tasks,
        )?;
        let api_url = format!("http://127.0.0.1:{api_port}");
        wait_http_ready(&api_url, &mut api_child, &logs).await?;

        let mut gw_child = spawn_remora_process(
            remora_bin,
            &gw_cfg,
            &["--bus-connect", &bus_str],
            &logs,
            &mut tasks,
        )?;
        wait_tcp_ready(
            (std::net::Ipv4Addr::LOCALHOST, gw_port).into(),
            &mut gw_child,
            &logs,
        )
        .await?;

        let proxy_task = spawn_split_proxy(proxy_listener, api_port, gw_port);
        tasks.push(proxy_task);

        Ok(Self {
            base_url,
            sign_key: parsed_sign_key,
            children: vec![api_child, gw_child],
            tasks,
            logs,
            _turn: turn_guard,
            _workspace: workspace,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn sign_key(&self) -> Option<&SignKey> {
        self.sign_key.as_ref()
    }

    pub fn logs(&self) -> String {
        self.logs.lock().expect("logs lock poisoned").clone()
    }

    pub async fn shutdown(mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        for mut child in self.children.drain(..) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

fn pick_free_port() -> Result<u16, LabError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn spawn_remora_process(
    remora_bin: &Path,
    config_path: &Path,
    extra_args: &[&str],
    logs: &Arc<Mutex<String>>,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<Child, LabError> {
    let mut cmd = Command::new(remora_bin);
    cmd.arg("--config")
        .arg(config_path)
        .args(extra_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|err| {
        LabError::Target(format!("failed to spawn {}: {err}", remora_bin.display()))
    })?;

    if let Some(stdout) = child.stdout.take() {
        let logs = Arc::clone(logs);
        tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut guard = logs.lock().expect("logs lock poisoned");
                guard.push_str(&line);
                guard.push('\n');
            }
        }));
    }
    if let Some(stderr) = child.stderr.take() {
        let logs = Arc::clone(logs);
        tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut guard = logs.lock().expect("logs lock poisoned");
                guard.push_str(&line);
                guard.push('\n');
            }
        }));
    }
    Ok(child)
}

async fn wait_http_ready(
    base_url: &str,
    child: &mut Child,
    logs: &Arc<Mutex<String>>,
) -> Result<(), LabError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .map_err(|e| LabError::Target(e.to_string()))?;
    let probe = format!("{base_url}/api/web/session");
    let deadline = Instant::now() + STARTUP_TIMEOUT;

    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            let log_tail = logs.lock().expect("logs lock poisoned").clone();
            return Err(LabError::Target(format!(
                "remora exited early with {status}: {log_tail}"
            )));
        }
        if let Ok(resp) = client.get(&probe).send().await
            && resp.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    let log_tail = logs.lock().expect("logs lock poisoned").clone();
    Err(LabError::Target(format!(
        "timed out waiting for remora HTTP readiness at {probe}; logs:\n{log_tail}"
    )))
}

async fn wait_tcp_ready(
    addr: SocketAddr,
    child: &mut Child,
    logs: &Arc<Mutex<String>>,
) -> Result<(), LabError> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            let log_tail = logs.lock().expect("logs lock poisoned").clone();
            return Err(LabError::Target(format!(
                "remora gateway exited early with {status}: {log_tail}"
            )));
        }
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    let log_tail = logs.lock().expect("logs lock poisoned").clone();
    Err(LabError::Target(format!(
        "timed out waiting for remora gateway TCP readiness at {addr}; logs:\n{log_tail}"
    )))
}

#[derive(Clone)]
struct ProxyState {
    client: reqwest::Client,
    api_base: String,
    gw_base_ws: String,
}

fn spawn_split_proxy(listener: TcpListener, api_port: u16, gw_port: u16) -> JoinHandle<()> {
    let state = ProxyState {
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("proxy HTTP client builds"),
        api_base: format!("http://127.0.0.1:{api_port}"),
        gw_base_ws: format!("ws://127.0.0.1:{gw_port}"),
    };

    let app = Router::new()
        .route("/socket.io/", axum::routing::get(proxy_socketio))
        .fallback(proxy_http)
        .with_state(state);

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    })
}

async fn proxy_socketio(
    ws: WebSocketUpgrade,
    State(state): State<ProxyState>,
    req: Request,
) -> Response {
    let path_q = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/socket.io/?EIO=4&transport=websocket");
    let target_url = format!("{}{path_q}", state.gw_base_ws);
    let Ok(mut upstream_req) = target_url.into_client_request() else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    for (name, value) in req.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "x-nrd-auth" || lower.starts_with("x-param-") || lower == "user-agent" {
            upstream_req
                .headers_mut()
                .insert(name.clone(), value.clone());
        }
    }
    let Ok((upstream_ws, _)) = tokio_tungstenite::connect_async(upstream_req).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    ws.on_upgrade(move |client_ws| bridge_ws(client_ws, upstream_ws))
}

async fn bridge_ws(
    client_ws: WebSocket,
    upstream_ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let (mut c_tx, mut c_rx) = client_ws.split();
    let (mut u_tx, mut u_rx) = upstream_ws.split();

    let c_to_u = async {
        while let Some(Ok(msg)) = c_rx.next().await {
            let mapped = match msg {
                AxumWsMsg::Text(t) => TungWsMsg::Text(t.to_string().into()),
                AxumWsMsg::Binary(b) => TungWsMsg::Binary(b.to_vec().into()),
                AxumWsMsg::Ping(p) => TungWsMsg::Ping(p.to_vec().into()),
                AxumWsMsg::Pong(p) => TungWsMsg::Pong(p.to_vec().into()),
                AxumWsMsg::Close(_) => {
                    let _ = u_tx.send(TungWsMsg::Close(None)).await;
                    break;
                }
            };
            if u_tx.send(mapped).await.is_err() {
                break;
            }
        }
    };

    let u_to_c = async {
        while let Some(Ok(msg)) = u_rx.next().await {
            let mapped = match msg {
                TungWsMsg::Text(t) => AxumWsMsg::Text(t.to_string().into()),
                TungWsMsg::Binary(b) => AxumWsMsg::Binary(b.to_vec().into()),
                TungWsMsg::Ping(p) => AxumWsMsg::Ping(p.to_vec().into()),
                TungWsMsg::Pong(p) => AxumWsMsg::Pong(p.to_vec().into()),
                TungWsMsg::Close(_) => {
                    let _ = c_tx.send(AxumWsMsg::Close(None)).await;
                    break;
                }
                TungWsMsg::Frame(_) => continue,
            };
            if c_tx.send(mapped).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        () = c_to_u => {}
        () = u_to_c => {}
    }
}

async fn proxy_http(State(state): State<ProxyState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path_q = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = format!("{}{path_q}", state.api_base);
    let Ok(bytes) = axum::body::to_bytes(body, 16 * 1024 * 1024).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut outgoing = state.client.request(parts.method, url);
    for (name, value) in &parts.headers {
        if !name.as_str().eq_ignore_ascii_case("host")
            && !name.as_str().eq_ignore_ascii_case("content-length")
        {
            outgoing = outgoing.header(name, value);
        }
    }
    let Ok(resp) = outgoing.body(bytes).send().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let status = resp.status();
    let headers = resp.headers().clone();
    let Ok(resp_bytes) = resp.bytes().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let mut builder = Response::builder().status(status);
    for (name, value) in &headers {
        if !name.as_str().eq_ignore_ascii_case("transfer-encoding")
            && !name.as_str().eq_ignore_ascii_case("content-length")
        {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from(resp_bytes))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}
