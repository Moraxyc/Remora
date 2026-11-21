//! Structural comparison between two [`NormalizedRun`] outcomes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::normalize::{NormalizedRun, NormalizedStep};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffMismatch {
    pub path: String,
    pub left: String,
    pub right: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffReport {
    pub scenario: String,
    pub left_target: String,
    pub right_target: String,
    pub identical: bool,
    pub mismatches: Vec<DiffMismatch>,
}

/// Compares two normalized scenario runs and returns any structural differences.
pub fn diff_runs(
    left_target: &str,
    left: &NormalizedRun,
    right_target: &str,
    right: &NormalizedRun,
) -> DiffReport {
    let mut mismatches = Vec::new();
    if left.passed != right.passed {
        mismatches.push(DiffMismatch {
            path: "passed".into(),
            left: left.passed.to_string(),
            right: right.passed.to_string(),
        });
    }
    if left.error != right.error {
        mismatches.push(DiffMismatch {
            path: "error".into(),
            left: format!("{:?}", left.error),
            right: format!("{:?}", right.error),
        });
    }
    if left.steps.len() != right.steps.len() {
        mismatches.push(DiffMismatch {
            path: "steps.len".into(),
            left: left.steps.len().to_string(),
            right: right.steps.len().to_string(),
        });
    }

    for (idx, (ls, rs)) in left.steps.iter().zip(right.steps.iter()).enumerate() {
        diff_step(idx, ls, rs, &mut mismatches);
    }

    DiffReport {
        scenario: left.scenario.clone(),
        left_target: left_target.to_owned(),
        right_target: right_target.to_owned(),
        identical: mismatches.is_empty(),
        mismatches,
    }
}

fn diff_step(
    idx: usize,
    left: &NormalizedStep,
    right: &NormalizedStep,
    mismatches: &mut Vec<DiffMismatch>,
) {
    let prefix = format!("steps[{idx}]");
    if left.actor != right.actor {
        mismatches.push(DiffMismatch {
            path: format!("{prefix}.actor"),
            left: left.actor.clone(),
            right: right.actor.clone(),
        });
    }
    if left.action != right.action {
        mismatches.push(DiffMismatch {
            path: format!("{prefix}.action"),
            left: left.action.clone(),
            right: right.action.clone(),
        });
    }
    if left.http_status != right.http_status {
        mismatches.push(DiffMismatch {
            path: format!("{prefix}.http_status"),
            left: format!("{:?}", left.http_status),
            right: format!("{:?}", right.http_status),
        });
    }
    if left.code != right.code {
        mismatches.push(DiffMismatch {
            path: format!("{prefix}.code"),
            left: format!("{:?}", left.code),
            right: format!("{:?}", right.code),
        });
    }
    if left.ack_status != right.ack_status {
        mismatches.push(DiffMismatch {
            path: format!("{prefix}.ack_status"),
            left: format!("{:?}", left.ack_status),
            right: format!("{:?}", right.ack_status),
        });
    }
    if left.event_name != right.event_name {
        mismatches.push(DiffMismatch {
            path: format!("{prefix}.event_name"),
            left: format!("{:?}", left.event_name),
            right: format!("{:?}", right.event_name),
        });
    }
    diff_value(
        &format!("{prefix}.payload"),
        &left.payload,
        &right.payload,
        mismatches,
    );
}

fn diff_value(path: &str, left: &Value, right: &Value, mismatches: &mut Vec<DiffMismatch>) {
    if left == right {
        return;
    }
    match (left, right) {
        (Value::Object(lm), Value::Object(rm)) => {
            for (k, lv) in lm {
                let child = format!("{path}.{k}");
                match rm.get(k) {
                    Some(rv) => diff_value(&child, lv, rv, mismatches),
                    None => mismatches.push(DiffMismatch {
                        path: child,
                        left: lv.to_string(),
                        right: "<missing>".into(),
                    }),
                }
            }
            for (k, rv) in rm {
                if !lm.contains_key(k) {
                    mismatches.push(DiffMismatch {
                        path: format!("{path}.{k}"),
                        left: "<missing>".into(),
                        right: rv.to_string(),
                    });
                }
            }
        }
        (Value::Array(la), Value::Array(ra)) => {
            if la.len() != ra.len() {
                mismatches.push(DiffMismatch {
                    path: format!("{path}.len"),
                    left: la.len().to_string(),
                    right: ra.len().to_string(),
                });
            }
            for (i, (lv, rv)) in la.iter().zip(ra.iter()).enumerate() {
                diff_value(&format!("{path}[{i}]"), lv, rv, mismatches);
            }
        }
        _ => mismatches.push(DiffMismatch {
            path: path.to_owned(),
            left: left.to_string(),
            right: right.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::normalize::normalize_run;
    use crate::scenario::ObservedStep;
    use crate::scenario::runner::ScenarioResult;

    #[test]
    fn reports_exact_field_mismatch() {
        let base = ScenarioResult {
            scenario: "s".into(),
            target: "left".into(),
            passed: true,
            error: None,
            trace: Vec::new(),
            duration_ms: 10,
            steps: vec![ObservedStep {
                index: 0,
                id: None,
                actor: "viewer".into(),
                action: "room_join".into(),
                http_status: Some(200),
                code: Some(0),
                ack_status: None,
                event_name: None,
                payload: json!({ "publisher_platform": 1 }),
            }],
        };
        let mut right_raw = base.clone();
        right_raw.steps[0].payload["publisher_platform"] = json!(4);

        let report = diff_runs(
            "remora (standalone)",
            &normalize_run(&base),
            "remora (split)",
            &normalize_run(&right_raw),
        );
        assert!(!report.identical);
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(
            report.mismatches[0].path,
            "steps[0].payload.publisher_platform"
        );
    }
}
