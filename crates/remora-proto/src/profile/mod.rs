//! Compile-time embedded official client profiles and capabilities.

pub mod detect;

pub use detect::detect_from_headers;

/// Operating system family of a UU Remote client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
    Ios,
    Android,
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
    use super::*;

    #[test]
    fn embeds_macos_profiles() {
        assert!(all().len() >= 2);
        let p481 = by_id("macos-4.48.1").expect("macos-4.48.1 embedded");
        assert_eq!(p481.platform, Platform::MacOs);
        assert_eq!(p481.plat, "4");
        assert_eq!(p481.version, "4.48.1");
        assert_eq!(p481.build, 633);
        assert_eq!(p481.init_endpoint, "/api/v1/device/macos/init");

        let p482 = by_id("macos-4.48.2").expect("macos-4.48.2 embedded");
        assert_eq!(p482.platform, Platform::MacOs);
        assert_eq!(p482.plat, "4");
        assert_eq!(p482.version, "4.48.2");
        assert_eq!(p482.build, 634);
        assert!(p482.caps.codecs.contains(&"h264"));
        assert!(p482.caps.data_channels.contains(&"CONTROL_DATA_CHANNEL"));
    }
}
