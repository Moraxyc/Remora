use std::path::PathBuf;

use remora_lab::diff::diff_runs;
use remora_lab::normalize::normalize_run;
use remora_lab::scenario::{Scenario, run_scenario};
use remora_lab::target::{RunningTarget, TargetSpec};

fn cloud_scenarios_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compat/scenarios/cloud")
}

#[tokio::test]
async fn all_cloud_scenarios_pass_against_fakecloud_and_normalize_identically() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(cloud_scenarios_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
        .collect();
    files.sort();
    assert!(!files.is_empty());

    for path in files {
        let scenario = Scenario::load(&path).unwrap();
        let target_a = RunningTarget::start(
            &TargetSpec::FakeCloud,
            None,
            Some("test-sign-key"),
            scenario.fakecloud.as_ref(),
        )
        .await
        .unwrap();
        let res_a = run_scenario(&scenario, &target_a, "fakecloud-a").await;
        target_a.shutdown().await;
        assert!(res_a.passed, "{}: {:?}", scenario.name, res_a.error);

        let target_b = RunningTarget::start(
            &TargetSpec::FakeCloud,
            None,
            Some("test-sign-key"),
            scenario.fakecloud.as_ref(),
        )
        .await
        .unwrap();
        let res_b = run_scenario(&scenario, &target_b, "fakecloud-b").await;
        target_b.shutdown().await;
        assert!(res_b.passed, "{}: {:?}", scenario.name, res_b.error);

        let diff = diff_runs(
            "fakecloud-a",
            &normalize_run(&res_a),
            "fakecloud-b",
            &normalize_run(&res_b),
        );
        assert!(
            diff.identical,
            "scenario {} normalized diff mismatch: {:?}",
            scenario.name, diff.mismatches
        );
    }
}

#[tokio::test]
async fn fakecloud_supports_scripted_http_overrides() {
    let yaml = r#"
name: fakecloud-override-test
profile: windows-4.48.2
actors:
  viewer:
    kind: viewer
fakecloud:
  http_overrides:
    - method: POST
      path: /api/v1/qrcode/gen/login
      status: 429
      code: 1429
      msg: rate limited
      data: { retry_after: 10 }
steps:
  - actor: viewer
    action: qr_generate
    expect:
      http_status: 429
      code: 1429
"#;
    let scenario = Scenario::from_yaml_str(yaml).unwrap();
    let target = RunningTarget::start(
        &TargetSpec::FakeCloud,
        None,
        Some("test-sign-key"),
        scenario.fakecloud.as_ref(),
    )
    .await
    .unwrap();
    let res = run_scenario(&scenario, &target, "fakecloud").await;
    let calls = match &target {
        RunningTarget::FakeCloud { instance, .. } => instance.recorded_http_calls(),
        _ => unreachable!(),
    };
    target.shutdown().await;

    assert!(res.passed, "{:?}", res.error);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "POST");
    assert_eq!(calls[0].path, "/api/v1/qrcode/gen/login");
}
