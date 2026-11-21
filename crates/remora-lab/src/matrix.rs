//! Compatibility matrix runner comparing `remora (standalone)` and `remora (split)`.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use remora_endpoint::Endpoint;
use remora_endpoint::role::{ViewerConfig, ViewerSession};
use remora_proto::control::SignKey;
use remora_proto::profile;
use remora_proto::session::input::InputMessage;
use serde::{Deserialize, Serialize};

use crate::diff::{DiffReport, diff_runs};
use crate::error::LabError;
use crate::headless::HeadlessPlatform;
use crate::normalize::normalize_run;
use crate::scenario::{Scenario, run_scenario};
use crate::target::{RunningTarget, TargetSpec};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixColumn {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixCell {
    pub column: String,
    pub passed: bool,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixRow {
    pub scenario: String,
    pub cells: Vec<MatrixCell>,
    pub diff_identical: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<DiffReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixReport {
    pub columns: Vec<MatrixColumn>,
    pub rows: Vec<MatrixRow>,
    pub all_passed: bool,
}

impl MatrixReport {
    pub fn to_json_pretty(&self) -> Result<String, LabError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("| Scenario |");
        for col in &self.columns {
            let _ = write!(out, " {} |", col.label);
        }
        out.push_str(" Diff |\n| --- |");
        for _ in &self.columns {
            out.push_str(" --- |");
        }
        out.push_str(" --- |\n");

        for row in &self.rows {
            let _ = write!(out, "| {} |", row.scenario);
            for cell in &row.cells {
                let mark = if cell.passed {
                    format!("PASS ({}ms)", cell.duration_ms)
                } else {
                    "FAIL".to_owned()
                };
                let _ = write!(out, " {mark} |");
            }
            let diff_mark = if row.diff_identical { "MATCH" } else { "DIFF" };
            let _ = writeln!(out, " {diff_mark} |");
        }
        out
    }
}

/// Executes all `scenarios` against both standalone and role-split Remora deployments,
/// normalizing and diffing their behavior.
pub async fn run_matrix(
    scenarios: &[Scenario],
    remora_bin: Option<&Path>,
    sign_key: Option<&str>,
) -> Result<MatrixReport, LabError> {
    let specs = [TargetSpec::RemoraStandalone, TargetSpec::RemoraSplit];
    let columns: Vec<MatrixColumn> = specs
        .iter()
        .map(|s| MatrixColumn {
            id: s.slug().to_owned(),
            label: s.label().to_owned(),
        })
        .collect();

    let mut rows = Vec::with_capacity(scenarios.len());
    let mut all_passed = true;

    for scenario in scenarios {
        let mut cells = Vec::with_capacity(specs.len());
        let mut normalized = Vec::with_capacity(specs.len());

        for spec in &specs {
            let target = RunningTarget::start(spec, remora_bin, sign_key, None).await?;
            let result = run_scenario(scenario, &target, spec.label()).await;
            target.shutdown().await;

            if !result.passed {
                all_passed = false;
            }
            cells.push(MatrixCell {
                column: spec.slug().to_owned(),
                passed: result.passed,
                duration_ms: result.duration_ms,
                error: result.error.clone(),
            });
            normalized.push((spec.label(), normalize_run(&result)));
        }

        let mut diffs = Vec::new();
        let mut diff_identical = true;
        if let Some((base_label, base_run)) = normalized.first() {
            for (other_label, other_run) in normalized.iter().skip(1) {
                let d = diff_runs(base_label, base_run, other_label, other_run);
                if !d.identical {
                    diff_identical = false;
                    all_passed = false;
                }
                diffs.push(d);
            }
        }

        rows.push(MatrixRow {
            scenario: scenario.name.clone(),
            cells,
            diff_identical,
            diffs,
        });
    }

    Ok(MatrixReport {
        columns,
        rows,
        all_passed,
    })
}

/// Configuration for [`run_session_matrix`] comparing `lab↔lab (P2P)`, `lab↔lab (relay)`,
/// and `lab↔uu-remote-linux`.
#[derive(Debug, Clone)]
pub struct SessionMatrixConfig<'a> {
    pub p2p_scenario: &'a Scenario,
    pub relay_scenario: &'a Scenario,
    pub url: Option<&'a str>,
    pub remora_bin: Option<&'a Path>,
    pub sign_key: Option<&'a str>,
    pub uu_device_id: Option<&'a str>,
    pub uu_user_token: Option<&'a str>,
    pub uu_force_relay: bool,
}

/// Executes the 3-column session compatibility matrix:
/// `lab↔lab (P2P)`, `lab↔lab (relay)`, and `lab↔uu-remote-linux`.
pub async fn run_session_matrix(cfg: SessionMatrixConfig<'_>) -> Result<MatrixReport, LabError> {
    let columns = vec![
        MatrixColumn {
            id: "lab-lab-p2p".into(),
            label: "lab↔lab (P2P)".into(),
        },
        MatrixColumn {
            id: "lab-lab-relay".into(),
            label: "lab↔lab (relay)".into(),
        },
        MatrixColumn {
            id: "lab-uu-remote-linux".into(),
            label: "lab↔uu-remote-linux".into(),
        },
    ];

    let spec = match cfg.url {
        Some(u) => TargetSpec::ExternalUrl(u.to_owned()),
        None => TargetSpec::RemoraStandalone,
    };

    let p2p_cell = {
        let target = RunningTarget::start(&spec, cfg.remora_bin, cfg.sign_key, None).await?;
        let res = run_scenario(cfg.p2p_scenario, &target, "lab↔lab (P2P)").await;
        target.shutdown().await;
        MatrixCell {
            column: "lab-lab-p2p".into(),
            passed: res.passed,
            duration_ms: res.duration_ms,
            error: res.error,
        }
    };

    let relay_cell = {
        let target = RunningTarget::start(&spec, cfg.remora_bin, cfg.sign_key, None).await?;
        let res = run_scenario(cfg.relay_scenario, &target, "lab↔lab (relay)").await;
        target.shutdown().await;
        MatrixCell {
            column: "lab-lab-relay".into(),
            passed: res.passed,
            duration_ms: res.duration_ms,
            error: res.error,
        }
    };

    let uu_cell = match (cfg.url, cfg.uu_device_id) {
        (Some(url), Some(dev_id)) => {
            let start = Instant::now();
            match run_uu_viewer_interop(
                url,
                cfg.sign_key,
                dev_id,
                cfg.uu_user_token,
                cfg.uu_force_relay,
            )
            .await
            {
                Ok(()) => MatrixCell {
                    column: "lab-uu-remote-linux".into(),
                    passed: true,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: None,
                },
                Err(err) => MatrixCell {
                    column: "lab-uu-remote-linux".into(),
                    passed: false,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: Some(err.to_string()),
                },
            }
        }
        _ => MatrixCell {
            column: "lab-uu-remote-linux".into(),
            passed: false,
            duration_ms: 0,
            error: Some("--url and --uu-device-id are required for lab↔uu-remote-linux".into()),
        },
    };

    let cells = vec![p2p_cell, relay_cell, uu_cell];
    let all_passed = cells.iter().all(|c| c.passed);

    Ok(MatrixReport {
        columns,
        rows: vec![MatrixRow {
            scenario: "desktop-session".into(),
            cells,
            diff_identical: all_passed,
            diffs: Vec::new(),
        }],
        all_passed,
    })
}

async fn run_uu_viewer_interop(
    base_url: &str,
    sign_key: Option<&str>,
    uu_device_id: &str,
    user_token: Option<&str>,
    force_relay: bool,
) -> Result<(), LabError> {
    let prof = profile::by_id("windows-4.48.2")
        .ok_or_else(|| LabError::Scenario("missing windows-4.48.2 profile".into()))?;
    let mut builder = Endpoint::builder(prof)
        .base_url(base_url)
        .device_name("remora-lab-uu-viewer")
        .auto_reconnect(false);
    if let Some(key) = sign_key {
        builder = builder.sign_key(SignKey::new(key));
    }
    if let Some(tok) = user_token {
        builder = builder.user_token(tok);
    }
    let endpoint = builder
        .build()
        .map_err(|e| LabError::Scenario(format!("build endpoint: {e}")))?;
    if user_token.is_none() {
        endpoint
            .login()
            .await
            .map_err(|e| LabError::Scenario(format!("uu interop login failed: {e}")))?;
    }

    let cfg = ViewerConfig {
        force_relay,
        connect_timeout: Duration::from_secs(15),
        ..Default::default()
    };

    let session = ViewerSession::connect(
        &endpoint,
        uu_device_id,
        Box::new(HeadlessPlatform::new()),
        cfg,
    )
    .await
    .map_err(|e| LabError::Scenario(format!("uu interop connect failed: {e}")))?;

    session
        .wait_ready(Duration::from_secs(15))
        .await
        .map_err(|e| LabError::Scenario(format!("uu interop echo failed: {e}")))?;

    let stats = session
        .wait_frames(3, Duration::from_secs(15))
        .await
        .map_err(|e| LabError::Scenario(format!("uu interop video frames failed: {e}")))?;
    if stats.video_frames_received < 3 {
        session.close().await;
        return Err(LabError::Scenario(format!(
            "expected >= 3 H.264 frames from uu-remote-linux, got {}",
            stats.video_frames_received
        )));
    }
    if force_relay && !stats.is_relayed {
        session.close().await;
        return Err(LabError::Scenario(
            "uu interop expected relayed ICE transport".into(),
        ));
    }

    session
        .send_input(InputMessage::Keyboard {
            key: 65,
            pressed: true,
        })
        .await
        .map_err(|e| LabError::Scenario(format!("uu interop input press failed: {e}")))?;
    session
        .send_input(InputMessage::Keyboard {
            key: 65,
            pressed: false,
        })
        .await
        .map_err(|e| LabError::Scenario(format!("uu interop input release failed: {e}")))?;
    session
        .send_clipboard("remora-lab-uu-clip")
        .await
        .map_err(|e| LabError::Scenario(format!("uu interop clipboard failed: {e}")))?;

    tokio::time::sleep(Duration::from_millis(200)).await;
    session.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_markdown_and_json_matrix_report() {
        let report = MatrixReport {
            columns: vec![
                MatrixColumn {
                    id: "remora-standalone".into(),
                    label: "remora (standalone)".into(),
                },
                MatrixColumn {
                    id: "remora-split".into(),
                    label: "remora (split)".into(),
                },
            ],
            rows: vec![MatrixRow {
                scenario: "01-login".into(),
                cells: vec![
                    MatrixCell {
                        column: "remora-standalone".into(),
                        passed: true,
                        duration_ms: 12,
                        error: None,
                    },
                    MatrixCell {
                        column: "remora-split".into(),
                        passed: true,
                        duration_ms: 18,
                        error: None,
                    },
                ],
                diff_identical: true,
                diffs: Vec::new(),
            }],
            all_passed: true,
        };
        let md = report.to_markdown();
        assert!(md.contains("| Scenario | remora (standalone) | remora (split) | Diff |"));
        assert!(md.contains("| 01-login | PASS (12ms) | PASS (18ms) | MATCH |"));
        let json_str = report.to_json_pretty().unwrap();
        let parsed: MatrixReport = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.all_passed);
    }

    #[test]
    fn formats_three_column_session_matrix() {
        let report = MatrixReport {
            columns: vec![
                MatrixColumn {
                    id: "lab-lab-p2p".into(),
                    label: "lab↔lab (P2P)".into(),
                },
                MatrixColumn {
                    id: "lab-lab-relay".into(),
                    label: "lab↔lab (relay)".into(),
                },
                MatrixColumn {
                    id: "lab-uu-remote-linux".into(),
                    label: "lab↔uu-remote-linux".into(),
                },
            ],
            rows: vec![MatrixRow {
                scenario: "desktop-session".into(),
                cells: vec![
                    MatrixCell {
                        column: "lab-lab-p2p".into(),
                        passed: true,
                        duration_ms: 190,
                        error: None,
                    },
                    MatrixCell {
                        column: "lab-lab-relay".into(),
                        passed: true,
                        duration_ms: 210,
                        error: None,
                    },
                    MatrixCell {
                        column: "lab-uu-remote-linux".into(),
                        passed: true,
                        duration_ms: 350,
                        error: None,
                    },
                ],
                diff_identical: true,
                diffs: Vec::new(),
            }],
            all_passed: true,
        };
        let md = report.to_markdown();
        assert!(md.contains(
            "| Scenario | lab↔lab (P2P) | lab↔lab (relay) | lab↔uu-remote-linux | Diff |"
        ));
        assert!(
            md.contains("| desktop-session | PASS (190ms) | PASS (210ms) | PASS (350ms) | MATCH |")
        );
    }
}
