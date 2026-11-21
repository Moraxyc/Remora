mod common;

use axum::Router;
use remora_proto::control::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct Init {
    device_id: String,
}

const PATH: &str = "/api/v1/device/windows/init";

async fn init(app: &Router, body: Value) -> Result<Init, ApiError> {
    common::post(app, PATH, Some(&body.to_string())).await
}

/// What uu-remote-linux sends, with synthetic values.
fn registration(client_id: &str) -> Value {
    json!({
        "name": "host", "client_id": client_id, "system_id": "SYSTEM", "machine_guid": "guid",
        "os": "Linux", "base_board": "board", "cpu": "cpu", "video": ["gpu"],
        "mac": "AA-BB-CC-DD-EE-FF", "memory": 16384, "screen": "1920x1080",
    })
}

#[tokio::test]
async fn registration_is_stable_per_installation() {
    let app = remora_cloud::router(common::config());
    let first = init(&app, registration("client-a")).await.unwrap();
    let again = init(&app, registration("client-a")).await.unwrap();
    let other = init(&app, registration("client-b")).await.unwrap();

    assert!(!first.device_id.is_empty());
    assert_eq!(first.device_id, again.device_id);
    assert_ne!(first.device_id, other.device_id);
}

const LIST: &str = "/api/v1/device/list";

#[tokio::test]
async fn the_list_holds_the_users_hosts() {
    let app = remora_cloud::router(common::config());
    let host = common::register_named(&app, "client-host", "studio").await;
    let viewer = common::register_named(&app, "client-viewer", "laptop").await;
    let unnamed = common::register_named(&app, "client-unnamed", "").await;
    let token = common::log_in(&app).await;

    let list: Value = common::get_as(&app, LIST, &token, &viewer).await.unwrap();
    assert_eq!(
        list["my_binded_devices"],
        json!([]),
        "no host has created a room"
    );
    assert_eq!(list["others_shared_devices"], json!([]));
    assert_eq!(list["current_device"]["device_id"], viewer.as_str());

    common::create_room(&app, &token, &host).await;
    common::create_room(&app, &token, &unnamed).await;
    let list: Value = common::get_as(&app, LIST, &token, &viewer).await.unwrap();
    let mut expected = [
        json!({
            "device_id": host,
            "alias": "studio",
            "platform": 1,
            "status": "DISCONNECTED",
            "participants_info": [],
            "version_name": "",
            "support_wol": false,
            "controllable": true,
            "publisher_availability_status": "offline",
            "controlled_support": true,
            "update_started_at": 0,
        }),
        json!({
            "device_id": unnamed,
            "platform": 1,
            "status": "DISCONNECTED",
            "participants_info": [],
            "version_name": "",
            "support_wol": false,
            "controllable": true,
            "publisher_availability_status": "offline",
            "controlled_support": true,
            "update_started_at": 0,
        }),
    ];
    expected.sort_by_key(|entry| entry["device_id"].as_str().unwrap().to_owned());
    assert_eq!(list["my_binded_devices"], json!(expected));
}

#[tokio::test]
async fn re_registering_renames_the_device() {
    let app = remora_cloud::router(common::config());
    let host = common::register_named(&app, "client-host", "old").await;
    let token = common::log_in(&app).await;
    common::create_room(&app, &token, &host).await;
    assert_eq!(
        common::register_named(&app, "client-host", "new").await,
        host
    );

    let list: Value = common::get_as(&app, LIST, &token, &host).await.unwrap();
    assert_eq!(list["my_binded_devices"][0]["alias"], "new");
}

#[tokio::test]
async fn the_list_needs_a_user_token() {
    let app = remora_cloud::router(common::config());
    let result = common::get_as::<Value>(&app, LIST, "bogus", "device").await;
    assert_eq!(result.map(drop), Err(ApiError::TokenExpired));
}

#[tokio::test]
async fn rejects_invalid_bodies_with_an_envelope() {
    let app = remora_cloud::router(common::config());
    let invalid = Err(ApiError::Rejected {
        http_status: 422,
        code: 1001,
    });

    assert_eq!(
        init(&app, json!({ "name": "host" })).await.map(drop),
        invalid
    );
    assert_eq!(init(&app, registration("")).await.map(drop), invalid);
    let not_json = common::post::<Value>(&app, PATH, Some("{")).await;
    assert_eq!(not_json.map(drop), invalid);
}

#[tokio::test]
async fn devices_and_logins_outlive_a_restart() {
    let dir = std::env::temp_dir().join(format!("remora-restart-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("remora.db");
    let _ = std::fs::remove_file(&path);
    let start = || {
        remora_cloud::router(remora_cloud::Config {
            database: remora_cloud::Database::open(&path).unwrap(),
            ..common::config()
        })
    };

    let app = start();
    let host = common::register(&app, "client-host").await;
    let token = common::log_in(&app).await;
    common::create_room(&app, &token, &host).await;
    drop(app);

    let app = start();
    let list: Value = common::get(&app, LIST, Some(&token)).await.unwrap();
    assert_eq!(list["my_binded_devices"][0]["device_id"], host.as_str());
    let again = init(&app, registration("client-host")).await.unwrap();
    assert_eq!(again.device_id, host);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn macos_init_groups_controllable_preferences_and_apps() {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use remora_proto::control::{DeviceGroupsResponse, DevicePreferencesResponse, parse_response};
    use tower::ServiceExt;

    let app = remora_cloud::router(common::config());
    let req = Request::post("/api/v1/device/macos/init")
        .header("content-type", "application/json")
        .header("x-param-vn", "4.48.1")
        .body(Body::from(
            json!({
                "client_id": "mac-client-1",
                "name": "MacBook",
                "controllable": false,
            })
            .to_string(),
        ))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let mac_init: Value = parse_response(status, &bytes).unwrap();
    let mac_id = mac_init["device_id"].as_str().unwrap().to_owned();
    assert_eq!(mac_init["platform"], 4);
    assert_eq!(mac_init["controllable"], false);
    assert_eq!(mac_init["version_name"], "4.48.1");
    assert_eq!(mac_init["publisher_availability_status"], "offline");

    let token = common::log_in(&app).await;
    common::create_room(&app, &token, &mac_id).await;

    let toggled: Value = common::post_as(
        &app,
        "/api/v1/device/mac_controllable",
        &token,
        Some(&mac_id),
        &json!({ "controllable": true }).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(toggled, json!({}));

    let groups: DeviceGroupsResponse =
        common::get_as(&app, "/api/v1/device/groups/of/my", &token, &mac_id)
            .await
            .unwrap();
    assert_eq!(groups.current_device_id.as_deref(), Some(mac_id.as_str()));
    assert_eq!(groups.desktop_devices.len(), 1);
    assert_eq!(groups.desktop_devices[0].device_id, mac_id);
    assert_eq!(groups.desktop_devices[0].controllable, Some(true));
    assert!(groups.mobile_devices.is_empty());

    let prefs: DevicePreferencesResponse = common::get_as(
        &app,
        &format!("/api/v1/device/{mac_id}/preferences"),
        &token,
        &mac_id,
    )
    .await
    .unwrap();
    assert_eq!(prefs.device_preferences.device_id, mac_id);
    assert!(!prefs.device_preferences.tools.sequence.is_empty());

    let app_payload = json!({
        "device_app_list": [{
            "dapp_id": "abc",
            "app_type": 2,
            "name": "Demo",
            "target_path": "/Applications/Demo.app",
            "app_status": 0,
            "last_launched_at": 0,
        }]
    });
    let updated: Value = common::post_as(
        &app,
        "/api/v1/app/list/of/current_device",
        &token,
        Some(&mac_id),
        &app_payload.to_string(),
    )
    .await
    .unwrap();
    assert_eq!(updated["device_app_list"].as_array().unwrap().len(), 1);

    let fetched: Value = common::get_as(
        &app,
        &format!("/api/v1/app/list/of/device/{mac_id}"),
        &token,
        &mac_id,
    )
    .await
    .unwrap();
    assert_eq!(fetched, updated);
}

#[tokio::test]
async fn client_compat_endpoints_answer_with_expected_envelopes() {
    let app = remora_cloud::router(common::config());
    let mac = common::register_named(&app, "client-mac", "Mac").await;
    let token = common::log_in(&app).await;

    let ent: Value = common::get_as(&app, "/api/v1/member/entitlements", &token, &mac)
        .await
        .unwrap();
    assert_eq!(
        ent["features"]["screen_quality"]["periods"][0]["limit"]["quality"],
        json!(["general", "hd", "bluray"])
    );

    let cfg: Value = common::post_as(
        &app,
        "/api/v1/tool/configures_with_version",
        &token,
        Some(&mac),
        &json!({
            "configures": [
                { "name": "app_viewscreen_limit", "version": "123" },
                { "name": "mac_h5_entrance_url_config", "version": "" },
            ]
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(cfg["app_viewscreen_limit"]["status"], 1);
    assert_eq!(cfg["mac_h5_entrance_url_config"]["status"], 2);

    let ui: Value = common::get_as(&app, "/api/v1/cloud_pc/ui_switch", &token, &mac)
        .await
        .unwrap();
    assert_eq!(ui, json!({ "switch": false }));

    let msg: Value = common::get_as(&app, "/api/v1/message/summary", &token, &mac)
        .await
        .unwrap();
    assert_eq!(msg, json!({ "no_read_num": 0 }));

    let strat: Value = common::get_as(
        &app,
        "/api/v1/campaign/strategies?scene=enter_home",
        &token,
        &mac,
    )
    .await
    .unwrap();
    assert_eq!(strat, json!({ "strategy": null }));

    let popups: Value = common::get_as(
        &app,
        "/api/v1/release/fetch/popups/info?source_version=4.26.0",
        &token,
        &mac,
    )
    .await
    .unwrap();
    assert_eq!(popups, json!({ "popups": [] }));

    let share: Value = common::get_as(&app, "/api/v1/device/share/info", &token, &mac)
        .await
        .unwrap();
    assert_eq!(share["connect_code"], "");
    assert_eq!(share["alias"], "Mac");
    let connect_id = share["connect_id"].as_str().unwrap();
    assert_eq!(connect_id.len(), 9);
    assert!(connect_id.bytes().all(|b| b.is_ascii_digit()));

    let upd: Value = common::get_as(&app, "/api/v1/release/fetch/update", &token, &mac)
        .await
        .unwrap();
    assert_eq!(upd["url"], "");
    assert_eq!(upd["update_type"], "NoUpdate");

    let _: Value = common::post_as(&app, "/api/v1/room/create", &token, Some(&mac), "{}")
        .await
        .unwrap();

    let dev_upd: Value = common::get_as(
        &app,
        &format!("/api/v1/release/fetch/update/{mac}?platform=1&version_name=4.45.0.4318"),
        &token,
        &mac,
    )
    .await
    .unwrap();
    assert_eq!(dev_upd["url"], "");
    assert_eq!(dev_upd["version_name"], "4.45.0.4318");
    assert_eq!(dev_upd["update_type"], "NoUpdate");
}
