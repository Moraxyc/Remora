use std::path::PathBuf;

use remora_lab::diff::diff_runs;
use remora_lab::normalize::normalize_run;
use remora_lab::scenario::{Scenario, run_scenario};
use remora_lab::target::{RunningTarget, TargetSpec};

fn cloud_scenarios_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compat/scenarios/cloud")
}

fn session_scenarios_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compat/scenarios/session")
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

#[tokio::test]
async fn tool_channels_session_scenario_passes_and_normalizes_identically() {
    let scenario = Scenario::load(session_scenarios_dir().join("03-tool-channels.yaml")).unwrap();

    let target_a = RunningTarget::start(&TargetSpec::FakeCloud, None, Some("test-sign-key"), None)
        .await
        .unwrap();
    let res_a = run_scenario(&scenario, &target_a, "fakecloud-a").await;
    target_a.shutdown().await;
    assert!(res_a.passed, "{}: {:?}", scenario.name, res_a.error);

    let target_b = RunningTarget::start(&TargetSpec::FakeCloud, None, Some("test-sign-key"), None)
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
        "03-tool-channels normalized diff mismatch: {:?}",
        diff.mismatches
    );
}

#[tokio::test]
async fn prefer_relay_falls_back_to_p2p_without_turn_while_force_relay_fails() {
    let prefer_yaml = r#"
name: prefer-relay-fallback
profile: windows-4.48.2
actors:
  host:
    kind: host
    device_name: fallback-host
  viewer:
    kind: viewer
    device_name: fallback-viewer
steps:
  - actor: host
    action: device_init
    expect: { http_status: 200, code: 0 }
  - actor: host
    action: login
    expect: { http_status: 200, code: 0 }
  - actor: host
    action: session_host_start
    prefer_relay: true
  - actor: viewer
    action: session_viewer_connect
    target: host
    token: "${host.user_token}"
    prefer_relay: true
    expect:
      ack_status: success
      json_eq:
        connected: true
        peer_ready: true
        is_relayed: false
  - actor: viewer
    action: session_wait
    min_frames: 2
    require_p2p: true
    expect:
      json_eq:
        min_frames_met: true
        is_relayed: false
  - actor: viewer
    action: session_close
  - actor: host
    action: session_close
"#;
    let prefer_scenario = Scenario::from_yaml_str(prefer_yaml).unwrap();
    let target = RunningTarget::start(&TargetSpec::FakeCloud, None, Some("test-sign-key"), None)
        .await
        .unwrap();
    let res = run_scenario(&prefer_scenario, &target, "fakecloud").await;
    target.shutdown().await;
    assert!(res.passed, "prefer_relay fallback failed: {:?}", res.error);

    let force_yaml = r#"
name: force-relay-without-turn-fails
profile: windows-4.48.2
actors:
  host:
    kind: host
    device_name: force-host
  viewer:
    kind: viewer
    device_name: force-viewer
steps:
  - actor: host
    action: device_init
    expect: { http_status: 200, code: 0 }
  - actor: host
    action: login
    expect: { http_status: 200, code: 0 }
  - actor: host
    action: session_host_start
    force_relay: true
  - actor: viewer
    action: session_viewer_connect
    target: host
    token: "${host.user_token}"
    force_relay: true
"#;
    let force_scenario = Scenario::from_yaml_str(force_yaml).unwrap();
    let target = RunningTarget::start(&TargetSpec::FakeCloud, None, Some("test-sign-key"), None)
        .await
        .unwrap();
    let res = run_scenario(&force_scenario, &target, "fakecloud").await;
    target.shutdown().await;
    assert!(!res.passed, "force_relay without TURN should fail");
    let err = res.error.unwrap_or_default();
    assert!(
        err.contains("force_relay") || err.contains("TURN") || err.contains("relay"),
        "unexpected error for force_relay without TURN: {err}"
    );
}
