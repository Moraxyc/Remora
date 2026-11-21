//! Compatibility matrix runner comparing `remora (standalone)` and `remora (split)`.

use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::diff::{DiffReport, diff_runs};
use crate::error::LabError;
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
}
