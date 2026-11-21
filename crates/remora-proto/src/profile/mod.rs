//! Compile-time embedded official client profiles and capabilities.

pub mod detect;
#[cfg(test)]
#[allow(dead_code)]
mod resolver;

pub use detect::detect_from_headers;

/// Operating system family of a UU Remote client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
    Ios,
    Android,
}

/// How the profile data was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProfileSource {
    /// Inferred or extrapolated from adjacent releases; evidence is optional.
    Inferred,
    /// Extracted by statically inspecting an official package or binary; requires evidence.
    ReverseEngineered,
    /// Observed from live wire captures against an official client; requires evidence.
    Captured,
}

/// Header-matching rule used to identify a client profile on incoming requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DetectRule {
    pub plat: &'static str,
    pub version: &'static str,
    pub build: Option<u32>,
}

/// Supported `FeatureFlag` revision levels advertised by a client build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureCaps {
    pub capture_setting: i32,
    pub simple_action: i32,
    pub system_metrics: i32,
    pub private_screen: i32,
    pub update_acquire: i32,
    pub file_transfer_ftp: i32,
    pub file_transfer_ftp2: i32,
    pub clipboard: i32,
    pub qos_stat: i32,
    pub mumu_control: i32,
    pub virtual_mouse_device: i32,
}

/// Wire and media capabilities of an official client build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub codecs: &'static [&'static str],
    pub audio_codecs: &'static [&'static str],
    pub transports: &'static [&'static str],
    pub data_channels: &'static [&'static str],
    pub features: FeatureCaps,
}

/// Version-specific transport and protocol quirks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quirks {
    pub kcp_quic_optional: bool,
    pub log_archive_push_only: bool,
    pub wallpaper_raw_post: bool,
}

/// Static description of an official client release, embedded from `profiles/*.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub id: &'static str,
    pub inherits: Option<&'static str>,
    pub source: ProfileSource,
    pub evidence: &'static [&'static str],
    pub platform: Platform,
    /// `X-Param-PLAT` wire value (e.g. `"4"` for macOS).
    pub plat: &'static str,
    /// `X-Param-VN` wire value (e.g. `"4.48.2"`).
    pub version: &'static str,
    /// `X-Param-VC` build number (e.g. `634`).
    pub build: u32,
    pub bundle_id: &'static str,
    pub channel: &'static str,
    pub min_os: &'static str,
    pub init_endpoint: &'static str,
    pub sign_headers: &'static [&'static str],
    pub detect: DetectRule,
    pub caps: Caps,
    pub quirks: Quirks,
}

include!(concat!(env!("OUT_DIR"), "/profiles.rs"));

/// All compile-time embedded client profiles.
pub fn all() -> &'static [Profile] {
    PROFILES
}

/// Looks up an embedded profile by its identifier (e.g. `"macos-4.48.2"`).
pub fn by_id(id: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| profile.id == id)
}

#[cfg(test)]
mod tests {
    use super::resolver::{CapsToml, FeaturesToml, ProfileToml, QuirksToml, resolve_all};
    use super::*;

    fn base_toml(id: &str, version: &str, build: u32) -> ProfileToml {
        ProfileToml {
            id: id.into(),
            inherits: None,
            source: Some("inferred".into()),
            evidence: Some(vec![]),
            platform: Some("macos".into()),
            plat: Some("4".into()),
            version: Some(version.into()),
            build: Some(build),
            bundle_id: Some("com.netease.uuremote".into()),
            channel: Some("nochannel".into()),
            min_os: Some("11.0".into()),
            init_endpoint: Some("/api/v1/device/macos/init".into()),
            sign_headers: Some(vec!["X-Param-PLAT".into()]),
            detect: None,
            caps: Some(CapsToml {
                codecs: Some(vec!["h264".into()]),
                audio_codecs: Some(vec!["opus".into()]),
                transports: Some(vec!["webrtc".into()]),
                data_channels: Some(vec!["CONTROL_DATA_CHANNEL".into()]),
                features: Some(FeaturesToml {
                    capture_setting: Some(6),
                    simple_action: Some(3),
                    system_metrics: Some(3),
                    private_screen: Some(2),
                    update_acquire: Some(1),
                    file_transfer_ftp: Some(2),
                    file_transfer_ftp2: Some(2),
                    clipboard: Some(3),
                    qos_stat: Some(1),
                    mumu_control: Some(1),
                    virtual_mouse_device: Some(1),
                }),
            }),
            quirks: Some(QuirksToml {
                kcp_quic_optional: Some(true),
                log_archive_push_only: Some(true),
                wallpaper_raw_post: Some(true),
            }),
        }
    }

    #[test]
    fn embeds_macos_profiles_with_inheritance() {
        assert!(all().len() >= 2);
        let p481 = by_id("macos-4.48.1").expect("macos-4.48.1 embedded");
        assert_eq!(p481.inherits, None);
        assert_eq!(p481.source, ProfileSource::ReverseEngineered);
        assert!(!p481.evidence.is_empty());
        assert_eq!(p481.platform, Platform::MacOs);
        assert_eq!(p481.build, 633);

        let p482 = by_id("macos-4.48.2").expect("macos-4.48.2 embedded");
        assert_eq!(p482.inherits, Some("macos-4.48.1"));
        assert_eq!(p482.source, ProfileSource::ReverseEngineered);
        assert_eq!(p482.build, 634);
        assert_eq!(p482.caps, p481.caps);
        assert_eq!(p482.quirks, p481.quirks);
        assert_eq!(p482.init_endpoint, p481.init_endpoint);
    }

    #[test]
    fn rejects_unknown_capability_names() {
        let mut p = base_toml("bad-cap", "4.48.1", 633);
        p.caps.as_mut().unwrap().codecs = Some(vec!["vp9".into()]);
        let err = resolve_all(vec![p]).unwrap_err();
        assert!(err.contains("unknown capability"), "{err}");
    }

    #[test]
    fn rejects_inherits_cycles() {
        let mut a = base_toml("a", "4.48.1", 633);
        a.inherits = Some("b".into());
        let mut b = base_toml("b", "4.48.2", 634);
        b.inherits = Some("a".into());
        let err = resolve_all(vec![a, b]).unwrap_err();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn rejects_duplicate_detect_rules() {
        let a = base_toml("a", "4.48.1", 633);
        let b = base_toml("b", "4.48.1", 633);
        let err = resolve_all(vec![a, b]).unwrap_err();
        assert!(err.contains("duplicate detect rule"), "{err}");
    }

    #[test]
    fn requires_evidence_only_for_reverse_engineered_and_captured() {
        let inferred = base_toml("inf", "4.48.0", 632);
        assert!(resolve_all(vec![inferred]).is_ok());

        let mut rev = base_toml("rev", "4.48.1", 633);
        rev.source = Some("reverse_engineered".into());
        let err = resolve_all(vec![rev]).unwrap_err();
        assert!(err.contains("evidence"), "{err}");

        let mut cap = base_toml("cap", "4.48.2", 634);
        cap.source = Some("captured".into());
        let err = resolve_all(vec![cap]).unwrap_err();
        assert!(err.contains("evidence"), "{err}");
    }
}
