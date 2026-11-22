//! Compile-time embedded official client profiles and capabilities.

use std::sync::LazyLock;

pub mod detect;
#[cfg(test)]
#[allow(dead_code)]
mod resolver;
mod types;

pub use detect::detect_from_headers;
pub use types::{
    Caps, DetectRule, FeatureCaps, Platform, Profile, ProfileSource, Quirks, SignalProfile,
};

static PROFILES: LazyLock<Vec<Profile>> = LazyLock::new(|| {
    serde_json::from_slice(include_bytes!(concat!(env!("OUT_DIR"), "/profiles.json")))
        .expect("embedded profiles.json must be valid")
});

/// All compile-time embedded client profiles.
pub fn all() -> &'static [Profile] {
    &PROFILES
}

/// Looks up an embedded profile by its identifier (e.g. `"macos-4.48.2"`).
pub fn by_id(id: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| profile.id == id)
}

#[cfg(test)]
mod tests {
    use super::resolver::resolve_all;
    use super::*;

    fn base_toml_str(id: &str, version: &str, build: u32) -> String {
        format!(
            r#"
id = "{id}"
source = "inferred"
evidence = []
platform = "macos"
plat = "4"
version = "{version}"
build = {build}
bundle_id = "com.netease.uuremote"
channel = "nochannel"
min_os = "11.0"
init_endpoint = "/api/v1/device/macos/init"
sign_headers = ["X-Param-PLAT"]

[caps]
codecs = ["h264"]
audio_codecs = ["opus"]
transports = ["webrtc"]
data_channels = ["CONTROL_DATA_CHANNEL"]

[caps.features]
capture_setting = 6
simple_action = 3
system_metrics = 3
private_screen = 2
update_acquire = 1
file_transfer_ftp = 2
file_transfer_ftp2 = 2
clipboard = 3
qos_stat = 1
mumu_control = 1
virtual_mouse_device = 1

[quirks]
kcp_quic_optional = true
log_archive_push_only = true
wallpaper_raw_post = true
"#
        )
    }

    fn parse_table(raw: &str) -> toml::Table {
        toml::from_str(raw).expect("valid test TOML")
    }

    #[test]
    fn embeds_macos_profiles_with_inheritance() {
        assert!(all().len() >= 2);
        let p481 = by_id("macos-4.48.1").expect("macos-4.48.1 embedded");
        assert_eq!(p481.inherits.as_deref(), None);
        assert_eq!(p481.source, ProfileSource::ReverseEngineered);
        assert!(!p481.evidence.is_empty());
        assert_eq!(p481.platform, Platform::MacOs);
        assert_eq!(p481.build, 633);

        let p482 = by_id("macos-4.48.2").expect("macos-4.48.2 embedded");
        assert_eq!(p482.inherits.as_deref(), Some("macos-4.48.1"));
        assert_eq!(p482.source, ProfileSource::ReverseEngineered);
        assert_eq!(p482.build, 634);
        assert_eq!(p482.caps, p481.caps);
        assert_eq!(p482.quirks, p481.quirks);
        assert_eq!(p482.init_endpoint, p481.init_endpoint);

        let p483 = by_id("macos-4.48.3").expect("macos-4.48.3 embedded");
        assert_eq!(p483.build, 635);
        assert!(p483.quirks.join_share_v1);
        assert!(p483.quirks.share_saved_favorites_only);

        let w435 = by_id("windows-4.35.0").expect("windows-4.35.0 embedded");
        assert!(!w435.quirks.device_preferences);
        assert!(w435.quirks.share_refresh_legacy);
        assert!(!w435.quirks.entitlements);

        let w445 = by_id("windows-4.45.0").expect("windows-4.45.0 embedded");
        assert!(w445.quirks.device_preferences);
        assert!(w445.quirks.share_refresh_legacy);
        assert!(!w445.quirks.entitlements);

        let w448 = by_id("windows-4.48.2").expect("windows-4.48.2 embedded");
        assert!(w448.quirks.entitlements);
        assert!(!w448.quirks.share_refresh_legacy);
        assert!(w448.quirks.guest_upload_control_mode);

        let a448 = by_id("android-4.48.2").expect("android-4.48.2 embedded");
        assert_eq!(a448.platform, Platform::Android);
        assert!(a448.quirks.response_sign_user_info);
        assert_eq!(a448.caps.features.capture_setting, 2);

        let tv350 = by_id("android-tv-3.5.0").expect("android-tv-3.5.0 embedded");
        assert_eq!(tv350.platform, Platform::AndroidTv);
        assert!(!tv350.quirks.kcp_quic_optional);
        assert!(!tv350.quirks.entitlements);
        assert_eq!(all().len(), 16);
    }

    #[test]
    fn embeds_captured_ios_profile() {
        let profile = by_id("ios-4.48.1").expect("ios-4.48.1 embedded");
        assert_eq!(profile.source, ProfileSource::Captured);
        assert_eq!(profile.platform, Platform::Ios);
        assert_eq!(profile.init_endpoint, "/api/v1/device/ios/init");
        assert_eq!(
            profile.detect,
            DetectRule {
                plat: "3".into(),
                version: "4.48.1".into(),
                build: Some(203),
            }
        );
        assert!(profile.quirks.response_sign_user_info);
    }

    #[test]
    fn rejects_unknown_capability_names() {
        let mut p = parse_table(&base_toml_str("bad-cap", "4.48.1", 633));
        p["caps"].as_table_mut().unwrap().insert(
            "codecs".into(),
            toml::Value::Array(vec![toml::Value::String("vp9".into())]),
        );
        let err = resolve_all(vec![p]).unwrap_err();
        assert!(err.contains("unknown capability"), "{err}");
    }

    #[test]
    fn rejects_inherits_cycles() {
        let mut a = parse_table(&base_toml_str("a", "4.48.1", 633));
        a.insert("inherits".into(), toml::Value::String("b".into()));
        let mut b = parse_table(&base_toml_str("b", "4.48.2", 634));
        b.insert("inherits".into(), toml::Value::String("a".into()));
        let err = resolve_all(vec![a, b]).unwrap_err();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn rejects_duplicate_detect_rules() {
        let a = parse_table(&base_toml_str("a", "4.48.1", 633));
        let b = parse_table(&base_toml_str("b", "4.48.1", 633));
        let err = resolve_all(vec![a, b]).unwrap_err();
        assert!(err.contains("duplicate detect rule"), "{err}");
    }

    #[test]
    fn requires_evidence_only_for_reverse_engineered_and_captured() {
        let inferred = parse_table(&base_toml_str("inf", "4.48.0", 632));
        assert!(resolve_all(vec![inferred]).is_ok());

        let mut rev = parse_table(&base_toml_str("rev", "4.48.1", 633));
        rev.insert(
            "source".into(),
            toml::Value::String("reverse_engineered".into()),
        );
        let err = resolve_all(vec![rev]).unwrap_err();
        assert!(err.contains("evidence"), "{err}");

        let mut cap = parse_table(&base_toml_str("cap", "4.48.2", 634));
        cap.insert("source".into(), toml::Value::String("captured".into()));
        let err = resolve_all(vec![cap]).unwrap_err();
        assert!(err.contains("evidence"), "{err}");
    }

    #[test]
    fn merges_nested_tables_and_defaults_signal_and_detect() {
        let parent = parse_table(&format!(
            "{}\n[signal]\nping_interval_ms = 10000\n",
            base_toml_str("parent", "4.48.1", 633)
        ));
        let child = parse_table(
            r#"
id = "child"
inherits = "parent"
version = "4.48.2"
build = 634

[caps.features]
simple_action = 4

[signal]
ping_timeout_ms = 5000
"#,
        );
        let resolved = resolve_all(vec![parent, child]).expect("merge should succeed");
        let child = &resolved[1];
        assert_eq!(child.caps.features.capture_setting, 6);
        assert_eq!(child.caps.features.simple_action, 4);
        assert_eq!(child.signal.ping_interval_ms, 10_000);
        assert_eq!(child.signal.ping_timeout_ms, 5_000);
        assert_eq!(child.signal.handshake_timeout_ms, 20_000);
        assert_eq!(child.detect.version, "4.48.2");
        assert_eq!(child.detect.build, Some(634));
    }
}
