use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use remora_lab::error::LabError;
use remora_lab::matrix::run_matrix;
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
    /// Run scenarios across `remora (standalone)` and `remora (split)` and diff normalized outputs.
    Matrix {
        /// Scenario YAML files or directories containing `.yaml`/`.yml` files.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Path to the `remora` cloud binary.
        #[arg(long)]
        remora_bin: Option<PathBuf>,
        /// Request signing key used by spawned targets.
        #[arg(long, default_value = "remora-lab-sign-key")]
        sign_key: String,
        /// Disable request signing on spawned targets.
        #[arg(long)]
        no_sign_key: bool,
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
            remora_bin,
            sign_key,
            no_sign_key,
            format,
            out_json,
            out_md,
        } => {
            let scenarios = load_scenarios(&paths)?;
            let effective_key = (!no_sign_key).then_some(sign_key.as_str());
            let report = run_matrix(&scenarios, remora_bin.as_deref(), effective_key).await?;
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
