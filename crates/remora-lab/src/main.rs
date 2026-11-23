use std::fs;
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use std::time::Duration;

use remora_endpoint::Endpoint;
use remora_endpoint::role::{HostConfig, HostSession, ToolEvent, ViewerConfig, ViewerSession};
use remora_proto::control::{JoinShareByCodeRequest, ShareControlModeRequest, SignKey};
use remora_proto::pb::{self, file_transfer_ftp_request, file_transfer_ftp_response, rpc_request};
use remora_proto::profile;
use remora_proto::session::DataChannel;
use remora_proto::session::control::ControlMessage;
use remora_proto::session::file::FileMessage;
use remora_proto::session::system::SystemMessage;
use remora_proto::session::terminal::TerminalControlMessage;
use remora_proto::session::tunnel::{TunnelFrame, TunnelFrameType, TunnelSyn, decode_syn_ack};

use remora_lab::error::LabError;
use remora_lab::headless::{
    AnnexBFileVideoSource, HeadlessPlatform, RecordingInputSink, spawn_headless_tool_responder,
    synthetic_annex_b_sample,
};
use remora_lab::matrix::{SessionMatrixConfig, run_matrix, run_session_matrix};
use remora_lab::scenario::{Scenario, ScenarioResult, run_scenario};
use remora_lab::target::{RunningTarget, TargetSpec};
use remora_lab::{probe, record, replay};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "remora-lab",
    version,
    about = "Conformance and scenario lab for Remora"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum RunTargetArg {
    Standalone,
    Split,
    Url,
    Fakecloud,
}

#[derive(Clone, Copy, ValueEnum)]
enum RunFormat {
    Pretty,
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum MatrixFormat {
    Markdown,
    Json,
    Both,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum MatrixMode {
    Cloud,
    Session,
}

#[derive(Subcommand)]
enum Command {
    /// Run one or more YAML scenarios against a target.
    Run {
        /// Scenario YAML files or directories containing `.yaml`/`.yml` files.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Target deployment to run scenarios against.
        #[arg(long, value_enum, default_value_t = RunTargetArg::Standalone)]
        target: RunTargetArg,
        /// Base URL when `--target url` is used (e.g. http://server:8680).
        #[arg(long)]
        url: Option<String>,
        /// Path to the `remora` cloud binary for standalone/split targets.
        #[arg(long)]
        remora_bin: Option<PathBuf>,
        /// Request signing key used by scenarios and spawned targets.
        #[arg(long, default_value = "remora-lab-sign-key")]
        sign_key: String,
        /// Disable request signing on spawned targets.
        #[arg(long)]
        no_sign_key: bool,
        /// Output format.
        #[arg(long, value_enum, default_value_t = RunFormat::Pretty)]
        format: RunFormat,
        /// Optional directory where JSONL wire traces are written (`<scenario>.jsonl`).
        #[arg(long)]
        trace_dir: Option<PathBuf>,
    },
    /// Run scenarios across compatibility columns and emit a matrix report.
    Matrix {
        /// Scenario YAML files or directories containing `.yaml`/`.yml` files.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Matrix mode (`cloud` for standalone vs split; `session` for P2P, relay, and uu-remote-linux).
        #[arg(long, value_enum, default_value_t = MatrixMode::Cloud)]
        mode: MatrixMode,
        /// External Remora base URL (used in `session` mode).
        #[arg(long)]
        url: Option<String>,
        /// Path to the `remora` cloud binary.
        #[arg(long)]
        remora_bin: Option<PathBuf>,
        /// Request signing key used by spawned targets.
        #[arg(long, default_value = "remora-lab-sign-key")]
        sign_key: String,
        /// Disable request signing on spawned targets.
        #[arg(long)]
        no_sign_key: bool,
        /// Remote `uuremote-server` device ID for `lab↔uu-remote-linux` in `session` mode.
        #[arg(long)]
        uu_device_id: Option<String>,
        /// Optional user token for `lab↔uu-remote-linux`.
        #[arg(long)]
        uu_user_token: Option<String>,
        /// Force TURN relay for the `lab↔uu-remote-linux` column.
        #[arg(long)]
        uu_force_relay: bool,
        /// Stdout format (`markdown`, `json`, or `both`).
        #[arg(long, value_enum, default_value_t = MatrixFormat::Markdown)]
        format: MatrixFormat,
        /// Optional path to write JSON matrix report.
        #[arg(long)]
        out_json: Option<PathBuf>,
        /// Optional path to write Markdown matrix report.
        #[arg(long)]
        out_md: Option<PathBuf>,
    },
    /// Serve a headless lab Host until expected inputs/frames are observed (for external viewer interop).
    ServeHost {
        /// Remora base URL (e.g. http://server:8680).
        #[arg(long)]
        url: String,
        /// Request signing key.
        #[arg(long, default_value = "remora-lab-sign-key")]
        sign_key: String,
        /// Disable request signing.
        #[arg(long)]
        no_sign_key: bool,
        /// Path to write `{"device_id":"..."}` once online.
        #[arg(long)]
        ready_file: PathBuf,
        /// Force TURN relay on the host peer.
        #[arg(long)]
        force_relay: bool,
        /// Minimum input events expected from the connecting viewer.
        #[arg(long, default_value_t = 1)]
        expect_inputs: usize,
        /// Minimum video frames sent to the connecting viewer.
        #[arg(long, default_value_t = 1)]
        expect_frames: u64,
        /// Maximum seconds to wait before failing.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
    },
    /// Connect to a remote-assistance host by connect ID (reading the code from stdin).
    AssistJoin {
        /// Remora base URL (e.g. http://server:8680).
        #[arg(long)]
        url: String,
        /// Request signing key.
        #[arg(long, default_value = "remora-lab-sign-key")]
        sign_key: String,
        /// Disable request signing.
        #[arg(long)]
        no_sign_key: bool,
        /// 9-digit remote-assistance share ID.
        #[arg(long)]
        connect_id: String,
        /// Force TURN relay on the viewer peer.
        #[arg(long)]
        force_relay: bool,
        /// Minimum video frames expected from the assisting host.
        #[arg(long, default_value_t = 1)]
        expect_frames: u64,
        /// Maximum seconds to wait before failing.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
    },
    /// Exercise file, terminal, tunnel, system, and relay preferences against a live host device.
    ToolInterop {
        /// Remora base URL (e.g. http://server:8680).
        #[arg(long)]
        url: String,
        /// Request signing key.
        #[arg(long, default_value = "remora-lab-sign-key")]
        sign_key: String,
        /// Disable request signing.
        #[arg(long)]
        no_sign_key: bool,
        /// Target host device ID.
        #[arg(long)]
        device_id: String,
        /// Force TURN relay on the viewer peer.
        #[arg(long)]
        force_relay: bool,
        /// Prefer TURN relay (falling back to direct ICE when unavailable).
        #[arg(long)]
        prefer_relay: bool,
        /// Require the negotiated ICE transport to be relayed.
        #[arg(long)]
        require_relayed: bool,
        /// Remote directory path to list via FileTransferFtp::ReadDir.
        #[arg(long, default_value = "/tmp")]
        read_dir: String,
        /// Optional TCP host reachable from the remote host for PortMappingFrame echo check.
        #[arg(long)]
        tunnel_host: Option<String>,
        /// Optional TCP port paired with `--tunnel-host`.
        #[arg(long)]
        tunnel_port: Option<u16>,
        /// Maximum seconds to wait per operation.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
    },
    /// Record live wire traces (placeholder).
    Record,
    /// Replay recorded observations through sans-IO state machines (placeholder).
    Replay,
    /// Probe target capabilities and headers (placeholder).
    Probe,
}

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cli = Cli::parse();
    match dispatch(cli.command).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("remora-lab: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cmd: Command) -> Result<bool, LabError> {
    match cmd {
        Command::Run {
            paths,
            target,
            url,
            remora_bin,
            sign_key,
            no_sign_key,
            format,
            trace_dir,
        } => {
            let scenarios = load_scenarios(&paths)?;
            let effective_key = (!no_sign_key).then_some(sign_key.as_str());
            let target_spec = match target {
                RunTargetArg::Standalone => TargetSpec::RemoraStandalone,
                RunTargetArg::Split => TargetSpec::RemoraSplit,
                RunTargetArg::Fakecloud => TargetSpec::FakeCloud,
                RunTargetArg::Url => {
                    let u = url.ok_or_else(|| {
                        LabError::Target("--url is required when --target url is selected".into())
                    })?;
                    TargetSpec::ExternalUrl(u)
                }
            };

            if let Some(dir) = &trace_dir {
                fs::create_dir_all(dir)?;
            }

            let mut results: Vec<ScenarioResult> = Vec::with_capacity(scenarios.len());
            let mut all_ok = true;

            match &target_spec {
                TargetSpec::ExternalUrl(_) => {
                    let running =
                        RunningTarget::start(&target_spec, None, effective_key, None).await?;
                    for scenario in &scenarios {
                        let res = run_scenario(scenario, &running, target_spec.label()).await;
                        if !res.passed {
                            all_ok = false;
                        }
                        if let Some(dir) = &trace_dir {
                            write_trace_file(dir, &res)?;
                        }
                        results.push(res);
                    }
                    running.shutdown().await;
                }
                _ => {
                    for scenario in &scenarios {
                        let running = RunningTarget::start(
                            &target_spec,
                            remora_bin.as_deref(),
                            effective_key,
                            scenario.fakecloud.as_ref(),
                        )
                        .await?;
                        let res = run_scenario(scenario, &running, target_spec.label()).await;
                        running.shutdown().await;
                        if !res.passed {
                            all_ok = false;
                        }
                        if let Some(dir) = &trace_dir {
                            write_trace_file(dir, &res)?;
                        }
                        results.push(res);
                    }
                }
            }

            match format {
                RunFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&results)?);
                }
                RunFormat::Pretty => {
                    for res in &results {
                        if res.passed {
                            println!(
                                "PASS {} [{}] ({} steps, {}ms)",
                                res.scenario,
                                res.target,
                                res.steps.len(),
                                res.duration_ms
                            );
                        } else {
                            println!(
                                "FAIL {} [{}]: {}",
                                res.scenario,
                                res.target,
                                res.error.as_deref().unwrap_or("unknown error")
                            );
                        }
                    }
                }
            }

            Ok(all_ok)
        }
        Command::Matrix {
            paths,
            mode,
            url,
            remora_bin,
            sign_key,
            no_sign_key,
            uu_device_id,
            uu_user_token,
            uu_force_relay,
            format,
            out_json,
            out_md,
        } => {
            let scenarios = load_scenarios(&paths)?;
            let effective_key = (!no_sign_key).then_some(sign_key.as_str());
            let report = match mode {
                MatrixMode::Cloud => {
                    run_matrix(&scenarios, remora_bin.as_deref(), effective_key).await?
                }
                MatrixMode::Session => {
                    let p2p = scenarios
                        .iter()
                        .find(|s| s.name.contains("p2p"))
                        .or_else(|| scenarios.first())
                        .ok_or_else(|| LabError::Scenario("missing p2p session scenario".into()))?;
                    let relay = scenarios
                        .iter()
                        .find(|s| s.name.contains("relay"))
                        .or_else(|| scenarios.get(1))
                        .ok_or_else(|| {
                            LabError::Scenario("missing relay session scenario".into())
                        })?;
                    run_session_matrix(SessionMatrixConfig {
                        p2p_scenario: p2p,
                        relay_scenario: relay,
                        url: url.as_deref(),
                        remora_bin: remora_bin.as_deref(),
                        sign_key: effective_key,
                        uu_device_id: uu_device_id.as_deref(),
                        uu_user_token: uu_user_token.as_deref(),
                        uu_force_relay,
                    })
                    .await?
                }
            };
            let json = report.to_json_pretty()?;
            let md = report.to_markdown();

            if let Some(path) = out_json {
                fs::write(path, &json)?;
            }
            if let Some(path) = out_md {
                fs::write(path, &md)?;
            }

            match format {
                MatrixFormat::Markdown => print!("{md}"),
                MatrixFormat::Json => println!("{json}"),
                MatrixFormat::Both => {
                    print!("{md}\n{json}\n");
                }
            }
            Ok(report.all_passed)
        }
        Command::ServeHost {
            url,
            sign_key,
            no_sign_key,
            ready_file,
            force_relay,
            expect_inputs,
            expect_frames,
            timeout_secs,
        } => {
            let effective_key = (!no_sign_key).then_some(sign_key.as_str());
            run_serve_host(
                &url,
                effective_key,
                &ready_file,
                force_relay,
                expect_inputs,
                expect_frames,
                Duration::from_secs(timeout_secs),
            )
            .await
        }
        Command::AssistJoin {
            url,
            sign_key,
            no_sign_key,
            connect_id,
            force_relay,
            expect_frames,
            timeout_secs,
        } => {
            let effective_key = (!no_sign_key).then_some(sign_key.as_str());
            run_assist_join(
                &url,
                effective_key,
                &connect_id,
                force_relay,
                expect_frames,
                Duration::from_secs(timeout_secs),
            )
            .await
        }
        Command::ToolInterop {
            url,
            sign_key,
            no_sign_key,
            device_id,
            force_relay,
            prefer_relay,
            require_relayed,
            read_dir,
            tunnel_host,
            tunnel_port,
            timeout_secs,
        } => {
            let effective_key = (!no_sign_key).then_some(sign_key.as_str());
            let tunnel = match (tunnel_host, tunnel_port) {
                (Some(host), Some(port)) => Some((host, port)),
                (None, None) => None,
                _ => {
                    return Err(LabError::Scenario(
                        "--tunnel-host and --tunnel-port must be specified together".into(),
                    ));
                }
            };
            run_tool_interop(
                &url,
                effective_key,
                &device_id,
                force_relay,
                prefer_relay,
                require_relayed,
                &read_dir,
                tunnel,
                Duration::from_secs(timeout_secs),
            )
            .await
        }
        Command::Record => {
            record::run_placeholder()?;
            Ok(true)
        }
        Command::Replay => {
            replay::run_placeholder()?;
            Ok(true)
        }
        Command::Probe => {
            probe::run_placeholder()?;
            Ok(true)
        }
    }
}

fn write_trace_file(dir: &Path, res: &ScenarioResult) -> Result<(), LabError> {
    let mut buf = String::new();
    for rec in &res.trace {
        buf.push_str(&serde_json::to_string(rec)?);
        buf.push('\n');
    }
    let path = dir.join(format!("{}.jsonl", res.scenario));
    fs::write(path, buf)?;
    Ok(())
}

fn load_scenarios(paths: &[PathBuf]) -> Result<Vec<Scenario>, LabError> {
    let mut files = Vec::new();
    for path in paths {
        collect_yaml_files(path, &mut files)?;
    }
    files.sort();
    if files.is_empty() {
        return Err(LabError::Scenario("no scenario YAML files found".into()));
    }
    let mut scenarios = Vec::with_capacity(files.len());
    for file in files {
        scenarios.push(Scenario::load(&file)?);
    }
    Ok(scenarios)
}

fn collect_yaml_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<(), LabError> {
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                collect_yaml_files(&entry, out)?;
            } else if entry
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml")
                })
            {
                out.push(entry);
            }
        }
        return Ok(());
    }
    Err(LabError::Scenario(format!(
        "scenario path does not exist: {}",
        path.display()
    )))
}

async fn run_serve_host(
    base_url: &str,
    sign_key: Option<&str>,
    ready_file: &Path,
    force_relay: bool,
    expect_inputs: usize,
    expect_frames: u64,
    timeout: Duration,
) -> Result<bool, LabError> {
    let prof = profile::by_id("windows-4.48.2")
        .ok_or_else(|| LabError::Scenario("missing windows-4.48.2 profile".into()))?;
    let mut builder = Endpoint::builder(prof)
        .base_url(base_url)
        .device_name("remora-lab-serve-host")
        .auto_reconnect(false);
    if let Some(key) = sign_key {
        builder = builder.sign_key(SignKey::new(key));
    }
    let endpoint = builder
        .build()
        .map_err(|e| LabError::Scenario(format!("build endpoint: {e}")))?;
    endpoint
        .register_device()
        .await
        .map_err(|e| LabError::Scenario(format!("serve-host register_device failed: {e}")))?;
    let login = endpoint
        .login()
        .await
        .map_err(|e| LabError::Scenario(format!("serve-host login failed: {e}")))?;

    let input_sink = RecordingInputSink::new();
    let video_src = AnnexBFileVideoSource::from_bytes(&synthetic_annex_b_sample())?.with_loop(true);
    let mut platform = HeadlessPlatform::new().with_video_source(video_src);
    platform.input_sink = input_sink.clone();

    let cfg = HostConfig {
        force_relay,
        ..Default::default()
    };
    let mut session = HostSession::start(&endpoint, Box::new(platform), cfg)
        .await
        .map_err(|e| LabError::Scenario(format!("serve-host start failed: {e}")))?;
    let tool_task = spawn_headless_tool_responder(&mut session);
    let device_id = session.device_id().to_owned();

    let tmp_ready = ready_file.with_extension("tmp");
    fs::write(
        &tmp_ready,
        serde_json::to_string(&serde_json::json!({ "device_id": device_id }))?,
    )?;
    fs::rename(&tmp_ready, ready_file)?;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut ever_ready = false;
    let ok = loop {
        let st = session.stats();
        ever_ready |= st.peer_ready || st.video_frames_sent > 0 || st.inputs_injected > 0;
        if ever_ready && st.video_frames_sent >= expect_frames && input_sink.len() >= expect_inputs
        {
            break true;
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!(
                "serve-host timed out: ever_ready={ever_ready} connected={} auth={} frames={} inputs={}",
                st.connected,
                st.authorized_sessions,
                st.video_frames_sent,
                input_sink.len()
            );
            break false;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    };

    session.close().await;
    if let Some(task) = tool_task {
        let _ = tokio::time::timeout(Duration::from_millis(250), task).await;
    }
    let _ = endpoint
        .http()
        .device_unbind(&login.token, &device_id, Some(&device_id))
        .await;
    Ok(ok)
}

#[allow(clippy::too_many_arguments)]
async fn run_tool_interop(
    base_url: &str,
    sign_key: Option<&str>,
    device_id: &str,
    force_relay: bool,
    prefer_relay: bool,
    require_relayed: bool,
    read_dir: &str,
    tunnel: Option<(String, u16)>,
    timeout: Duration,
) -> Result<bool, LabError> {
    let prof = profile::by_id("windows-4.48.2")
        .ok_or_else(|| LabError::Scenario("missing windows-4.48.2 profile".into()))?;
    let mut builder = Endpoint::builder(prof)
        .base_url(base_url)
        .device_name("remora-lab-tool-interop")
        .auto_reconnect(false);
    if let Some(key) = sign_key {
        builder = builder.sign_key(SignKey::new(key));
    }
    let endpoint = builder
        .build()
        .map_err(|e| LabError::Scenario(format!("build endpoint: {e}")))?;
    endpoint
        .login()
        .await
        .map_err(|e| LabError::Scenario(format!("tool-interop login failed: {e}")))?;

    let cfg = ViewerConfig {
        force_relay,
        prefer_relay,
        connect_timeout: timeout,
        ..Default::default()
    };
    let mut session =
        ViewerSession::connect(&endpoint, device_id, Box::new(HeadlessPlatform::new()), cfg)
            .await
            .map_err(|e| LabError::Scenario(format!("tool-interop connect failed: {e}")))?;
    let mut tool_rx = session
        .take_tool_events()
        .ok_or_else(|| LabError::Scenario("missing viewer tool_events receiver".into()))?;

    let stats = session
        .wait_tool_channels_ready(timeout)
        .await
        .map_err(|e| LabError::Scenario(format!("tool-interop channels ready failed: {e}")))?;
    if require_relayed && !stats.is_relayed {
        session.close().await;
        return Err(LabError::Scenario(
            "tool-interop expected relayed ICE transport".into(),
        ));
    }

    let result = async {
        // 1. File channel: ReadDir sent on CONTROL, reply expected on FILE.
        session
            .send_file_request(
                DataChannel::Control,
                101,
                file_transfer_ftp_request::Which::ReadDir(pb::ReadDir {
                    id: Some(pb::TaskId {
                        task_id: 1,
                        file_index: 0,
                    }),
                    path: read_dir.to_owned(),
                }),
            )
            .await
            .map_err(|e| LabError::Scenario(format!("send read_dir failed: {e}")))?;
        let file_err = wait_for_tool(&mut tool_rx, timeout, |ev| match ev {
            ToolEvent::File(msg) => match msg.as_ref() {
                FileMessage::Response {
                    request_id: 101,
                    response,
                    ..
                } => match &response.which {
                    Some(file_transfer_ftp_response::Which::FileDirectory(dir)) => {
                        Some(dir.file_error)
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .await?;
        if file_err != pb::FileTransferError::FtSucceeded as i32 {
            return Err(LabError::Scenario(format!(
                "tool-interop read_dir returned file_error={file_err}"
            )));
        }

        // 2. Terminal control RPC: CheckEnvReq -> CheckEnvResp(code=0).
        session
            .send_control(ControlMessage::RpcRequest {
                seq: 102,
                timestamp: 0,
                request: pb::RpcRequest {
                    request_header: Some(pb::RequestHeader { request_id: 102 }),
                    which: Some(rpc_request::Which::TerminalCheckEnvReq(pb::CheckEnvReq {})),
                },
            })
            .await
            .map_err(|e| LabError::Scenario(format!("send check_env failed: {e}")))?;
        let env_code = wait_for_tool(&mut tool_rx, timeout, |ev| match ev {
            ToolEvent::TerminalControl(TerminalControlMessage::CheckEnvResponse {
                request_id: 102,
                code,
                ..
            }) => Some(*code),
            _ => None,
        })
        .await?;
        if env_code != 0 {
            return Err(LabError::Scenario(format!(
                "tool-interop check_env returned code={env_code}"
            )));
        }

        // 3. System RPC: VirtualAudioDriverPolicyReq -> VirtualAudioPolicyResponse(error_code=0).
        session
            .send_control(ControlMessage::RpcRequest {
                seq: 103,
                timestamp: 0,
                request: pb::RpcRequest {
                    request_header: Some(pb::RequestHeader { request_id: 103 }),
                    which: Some(rpc_request::Which::VirtualAudioDriverPolicyReq(
                        pb::VirtualAudioDriverPolicyRequest { policy: 0 },
                    )),
                },
            })
            .await
            .map_err(|e| LabError::Scenario(format!("send virtual audio probe failed: {e}")))?;
        let audio_err = wait_for_tool(&mut tool_rx, timeout, |ev| match ev {
            ToolEvent::System(msg) => match msg.as_ref() {
                SystemMessage::VirtualAudioPolicyResponse {
                    request_id: 103,
                    error_code,
                    ..
                } => Some(*error_code),
                _ => None,
            },
            _ => None,
        })
        .await?;
        if audio_err != 0 {
            return Err(LabError::Scenario(format!(
                "tool-interop virtual audio probe returned error_code={audio_err}"
            )));
        }

        // 4. Optional PortMappingFrame tunnel echo check against a live TCP endpoint.
        if let Some((host, port)) = tunnel {
            let syn_payload = serde_json::to_vec(&TunnelSyn {
                version: 1,
                target_host: host,
                target_port: port,
            })?;
            session
                .send_tunnel_frame(TunnelFrame {
                    seq: 104,
                    timestamp: 0,
                    session_id: 1,
                    rule_id: 1,
                    stream_id: 1,
                    frame_type: TunnelFrameType::Syn,
                    payload: syn_payload,
                })
                .await
                .map_err(|e| LabError::Scenario(format!("send tunnel syn failed: {e}")))?;
            let syn_ok = wait_for_tool(&mut tool_rx, timeout, |ev| match ev {
                ToolEvent::Tunnel(f)
                    if f.session_id == 1
                        && f.rule_id == 1
                        && f.stream_id == 1
                        && f.frame_type == TunnelFrameType::SynAck =>
                {
                    decode_syn_ack(&f.payload).ok().map(|ack| ack.ok)
                }
                _ => None,
            })
            .await?;
            if !syn_ok {
                return Err(LabError::Scenario(
                    "tool-interop tunnel SYN_ACK returned ok=false".into(),
                ));
            }

            let probe = b"remora-lab-tunnel-ok
"
            .to_vec();
            session
                .send_tunnel_frame(TunnelFrame {
                    seq: 105,
                    timestamp: 0,
                    session_id: 1,
                    rule_id: 1,
                    stream_id: 1,
                    frame_type: TunnelFrameType::Data,
                    payload: probe.clone(),
                })
                .await
                .map_err(|e| LabError::Scenario(format!("send tunnel data failed: {e}")))?;

            let mut received = Vec::new();
            let deadline = tokio::time::Instant::now() + timeout;
            while received.len() < probe.len() {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let chunk = wait_for_tool(&mut tool_rx, remaining, |ev| match ev {
                    ToolEvent::Tunnel(f)
                        if f.session_id == 1
                            && f.rule_id == 1
                            && f.stream_id == 1
                            && f.frame_type == TunnelFrameType::Data =>
                    {
                        Some(f.payload.clone())
                    }
                    _ => None,
                })
                .await?;
                received.extend_from_slice(&chunk);
            }
            if received != probe {
                return Err(LabError::Scenario(format!(
                    "tool-interop tunnel echo mismatch: got {} bytes",
                    received.len()
                )));
            }

            session
                .send_tunnel_frame(TunnelFrame {
                    seq: 106,
                    timestamp: 0,
                    session_id: 1,
                    rule_id: 1,
                    stream_id: 1,
                    frame_type: TunnelFrameType::Fin,
                    payload: Vec::new(),
                })
                .await
                .map_err(|e| LabError::Scenario(format!("send tunnel fin failed: {e}")))?;
        }

        Ok::<(), LabError>(())
    }
    .await;

    session.close().await;
    result?;
    Ok(true)
}

async fn wait_for_tool<T>(
    rx: &mut tokio::sync::mpsc::Receiver<ToolEvent>,
    timeout: Duration,
    mut matcher: impl FnMut(&ToolEvent) -> Option<T>,
) -> Result<T, LabError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(ev)) => {
                if let Some(val) = matcher(&ev) {
                    return Ok(val);
                }
            }
            Ok(None) => {
                return Err(LabError::Scenario(
                    "tool_events channel closed while waiting for reply".into(),
                ));
            }
            Err(_) => {
                return Err(LabError::Scenario(format!(
                    "timed out waiting for tool reply after {}ms",
                    timeout.as_millis()
                )));
            }
        }
    }
}

async fn run_assist_join(
    base_url: &str,
    sign_key: Option<&str>,
    connect_id: &str,
    force_relay: bool,
    expect_frames: u64,
    timeout: Duration,
) -> Result<bool, LabError> {
    let mut code = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut code)
        .map_err(|e| LabError::Scenario(format!("failed to read assist code from stdin: {e}")))?;
    let code = code.trim().to_owned();
    if code.is_empty() {
        return Err(LabError::Scenario(
            "empty remote-assistance verification code on stdin".into(),
        ));
    }

    let prof = profile::by_id("windows-4.48.2")
        .ok_or_else(|| LabError::Scenario("missing windows-4.48.2 profile".into()))?;
    let mut builder = Endpoint::builder(prof)
        .base_url(base_url)
        .device_name("remora-lab-assist-viewer")
        .auto_reconnect(false);
    if let Some(key) = sign_key {
        builder = builder.sign_key(SignKey::new(key));
    }
    let endpoint = builder
        .build()
        .map_err(|e| LabError::Scenario(format!("build endpoint: {e}")))?;
    endpoint
        .register_device()
        .await
        .map_err(|e| LabError::Scenario(format!("assist-join register_device failed: {e}")))?;
    endpoint
        .login()
        .await
        .map_err(|e| LabError::Scenario(format!("assist-join login failed: {e}")))?;

    let mode = endpoint
        .room_share_control_mode(&ShareControlModeRequest {
            connect_id: connect_id.to_owned(),
            control_id: None,
        })
        .await
        .map_err(|e| LabError::Scenario(format!("assist-join control_mode failed: {e}")))?;
    if !mode.is_allowed() {
        return Err(LabError::Scenario(
            "assist host reported can_remote_control=false".into(),
        ));
    }

    let assist_room = endpoint
        .room_join_share_by_code(&JoinShareByCodeRequest {
            connect_id: connect_id.to_owned(),
            connect_code: code,
            force_join: false,
        })
        .await
        .map_err(|e| LabError::Scenario(format!("assist-join by_code failed: {e}")))?;

    let cfg = ViewerConfig {
        force_relay,
        connect_timeout: timeout,
        ..Default::default()
    };
    let session = ViewerSession::connect_assist(
        &endpoint,
        &assist_room,
        Box::new(HeadlessPlatform::new()),
        cfg,
    )
    .await
    .map_err(|e| LabError::Scenario(format!("assist-join session connect failed: {e}")))?;

    let deadline = tokio::time::Instant::now() + timeout;
    let ok = loop {
        let st = session.stats();
        if st.peer_ready && st.video_frames_received >= expect_frames {
            break true;
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!(
                "assist-join timed out: connected={} control_open={} peer_ready={} frames={}",
                st.connected, st.control_open, st.peer_ready, st.video_frames_received
            );
            break false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    session.close().await;
    Ok(ok)
}
