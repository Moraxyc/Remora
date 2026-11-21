use std::collections::{HashMap, HashSet};

use serde::Deserialize;

pub const KNOWN_CODECS: &[&str] = &["h264", "h265"];
pub const KNOWN_AUDIO_CODECS: &[&str] = &["opus"];
pub const KNOWN_TRANSPORTS: &[&str] = &["webrtc", "mix-kcp", "quic"];
pub const KNOWN_DATA_CHANNELS: &[&str] = &[
    "CONTROL_DATA_CHANNEL",
    "TEXT_DATA_CHANNEL",
    "FILE_DATA_CHANNEL",
    "BINARY_DATA_CHANNEL",
    "STREAMER_DATA_CHANNEL",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileToml {
    pub id: String,
    #[serde(default)]
    pub inherits: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub evidence: Option<Vec<String>>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub plat: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub build: Option<u32>,
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub min_os: Option<String>,
    #[serde(default)]
    pub init_endpoint: Option<String>,
    #[serde(default)]
    pub sign_headers: Option<Vec<String>>,
    #[serde(default)]
    pub detect: Option<DetectToml>,
    #[serde(default)]
    pub caps: Option<CapsToml>,
    #[serde(default)]
    pub quirks: Option<QuirksToml>,
    #[serde(default)]
    pub signal: Option<SignalToml>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalToml {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub handshake_timeout_ms: Option<u64>,
    #[serde(default)]
    pub ping_interval_ms: Option<u64>,
    #[serde(default)]
    pub ping_timeout_ms: Option<u64>,
    #[serde(default)]
    pub reconnect_initial_ms: Option<u64>,
    #[serde(default)]
    pub reconnect_max_ms: Option<u64>,
    #[serde(default)]
    pub reconnect_key_refresh_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectToml {
    #[serde(default)]
    pub plat: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub build: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsToml {
    #[serde(default)]
    pub codecs: Option<Vec<String>>,
    #[serde(default)]
    pub audio_codecs: Option<Vec<String>>,
    #[serde(default)]
    pub transports: Option<Vec<String>>,
    #[serde(default)]
    pub data_channels: Option<Vec<String>>,
    #[serde(default)]
    pub features: Option<FeaturesToml>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeaturesToml {
    #[serde(default)]
    pub capture_setting: Option<i32>,
    #[serde(default)]
    pub simple_action: Option<i32>,
    #[serde(default)]
    pub system_metrics: Option<i32>,
    #[serde(default)]
    pub private_screen: Option<i32>,
    #[serde(default)]
    pub update_acquire: Option<i32>,
    #[serde(default)]
    pub file_transfer_ftp: Option<i32>,
    #[serde(default)]
    pub file_transfer_ftp2: Option<i32>,
    #[serde(default)]
    pub clipboard: Option<i32>,
    #[serde(default)]
    pub qos_stat: Option<i32>,
    #[serde(default)]
    pub mumu_control: Option<i32>,
    #[serde(default)]
    pub virtual_mouse_device: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuirksToml {
    #[serde(default)]
    pub kcp_quic_optional: Option<bool>,
    #[serde(default)]
    pub log_archive_push_only: Option<bool>,
    #[serde(default)]
    pub wallpaper_raw_post: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub id: String,
    pub inherits: Option<String>,
    pub source: &'static str,
    pub evidence: Vec<String>,
    pub platform: &'static str,
    pub plat: String,
    pub version: String,
    pub build: u32,
    pub bundle_id: String,
    pub channel: String,
    pub min_os: String,
    pub init_endpoint: String,
    pub sign_headers: Vec<String>,
    pub detect_plat: String,
    pub detect_version: String,
    pub detect_build: Option<u32>,
    pub codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
    pub transports: Vec<String>,
    pub data_channels: Vec<String>,
    pub features: ResolvedFeatures,
    pub quirks: ResolvedQuirks,
    pub signal: ResolvedSignal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSignal {
    pub path: String,
    pub handshake_timeout_ms: u64,
    pub ping_interval_ms: u64,
    pub ping_timeout_ms: u64,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,
    pub reconnect_key_refresh_ms: u64,
}

impl Default for ResolvedSignal {
    fn default() -> Self {
        Self {
            path: "/socket.io/?EIO=4&transport=websocket".to_owned(),
            handshake_timeout_ms: 20_000,
            ping_interval_ms: 25_000,
            ping_timeout_ms: 20_000,
            reconnect_initial_ms: 1_000,
            reconnect_max_ms: 30_000,
            reconnect_key_refresh_ms: 300_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFeatures {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedQuirks {
    pub kcp_quic_optional: bool,
    pub log_archive_push_only: bool,
    pub wallpaper_raw_post: bool,
}

pub fn resolve_all(inputs: Vec<ProfileToml>) -> Result<Vec<ResolvedProfile>, String> {
    let mut by_id = HashMap::new();
    let mut order = Vec::with_capacity(inputs.len());
    for raw in inputs {
        if raw.id.trim().is_empty() {
            return Err("profile id must not be empty".into());
        }
        if by_id.contains_key(&raw.id) {
            return Err(format!("duplicate profile id {:?}", raw.id));
        }
        order.push(raw.id.clone());
        by_id.insert(raw.id.clone(), raw);
    }

    let mut cache: HashMap<String, ResolvedProfile> = HashMap::new();
    let mut resolved = Vec::with_capacity(order.len());
    for id in &order {
        let mut stack = Vec::new();
        let profile = resolve_one(id, &by_id, &mut cache, &mut stack)?;
        resolved.push(profile);
    }

    check_duplicate_detect_rules(&resolved)?;
    Ok(resolved)
}

fn resolve_one(
    id: &str,
    by_id: &HashMap<String, ProfileToml>,
    cache: &mut HashMap<String, ResolvedProfile>,
    stack: &mut Vec<String>,
) -> Result<ResolvedProfile, String> {
    if let Some(done) = cache.get(id) {
        return Ok(done.clone());
    }
    if stack.iter().any(|item| item == id) {
        stack.push(id.to_owned());
        return Err(format!("profile inherits cycle: {}", stack.join(" -> ")));
    }
    let raw = by_id
        .get(id)
        .ok_or_else(|| format!("unknown inherited profile {id:?}"))?;

    stack.push(id.to_owned());
    let parent = match &raw.inherits {
        Some(parent_id) => Some(resolve_one(parent_id, by_id, cache, stack)?),
        None => None,
    };
    stack.pop();

    let profile = merge_profile(raw, parent.as_ref())?;
    cache.insert(id.to_owned(), profile.clone());
    Ok(profile)
}

fn req<'a, T: Clone>(
    child: Option<&'a T>,
    parent: Option<&'a T>,
    id: &str,
    field: &str,
) -> Result<T, String> {
    child
        .or(parent)
        .cloned()
        .ok_or_else(|| format!("profile {id:?} missing required field {field:?}"))
}

fn merge_profile(
    raw: &ProfileToml,
    parent: Option<&ResolvedProfile>,
) -> Result<ResolvedProfile, String> {
    let id = raw.id.as_str();
    let source_str = match (&raw.source, parent) {
        (Some(s), _) => s.as_str(),
        (None, Some(p)) => match p.source {
            "ProfileSource::Inferred" => "inferred",
            "ProfileSource::ReverseEngineered" => "reverse_engineered",
            "ProfileSource::Captured" => "captured",
            other => other,
        },
        (None, None) => {
            return Err(format!("profile {id:?} missing required field \"source\""));
        }
    };
    let source = match source_str {
        "inferred" => "ProfileSource::Inferred",
        "reverse_engineered" => "ProfileSource::ReverseEngineered",
        "captured" => "ProfileSource::Captured",
        other => {
            return Err(format!(
                "profile {id:?} has unknown source {other:?} (expected inferred | reverse_engineered | captured)"
            ));
        }
    };
    let evidence = raw
        .evidence
        .clone()
        .or_else(|| parent.map(|p| p.evidence.clone()))
        .ok_or_else(|| format!("profile {id:?} missing required field \"evidence\""))?;
    if matches!(source_str, "reverse_engineered" | "captured")
        && (evidence.is_empty() || evidence.iter().any(|e| e.trim().is_empty()))
    {
        return Err(format!(
            "profile {id:?} with source {source_str:?} must have non-empty evidence"
        ));
    }

    let platform = match (&raw.platform, parent) {
        (Some(p), _) => map_platform(p, id)?,
        (None, Some(p)) => p.platform,
        (None, None) => {
            return Err(format!(
                "profile {id:?} missing required field \"platform\""
            ));
        }
    };

    let plat = req(raw.plat.as_ref(), parent.map(|p| &p.plat), id, "plat")?;
    let version = req(
        raw.version.as_ref(),
        parent.map(|p| &p.version),
        id,
        "version",
    )?;
    let build = req(raw.build.as_ref(), parent.map(|p| &p.build), id, "build")?;
    let bundle_id = req(
        raw.bundle_id.as_ref(),
        parent.map(|p| &p.bundle_id),
        id,
        "bundle_id",
    )?;
    let channel = req(
        raw.channel.as_ref(),
        parent.map(|p| &p.channel),
        id,
        "channel",
    )?;
    let min_os = req(raw.min_os.as_ref(), parent.map(|p| &p.min_os), id, "min_os")?;
    let init_endpoint = req(
        raw.init_endpoint.as_ref(),
        parent.map(|p| &p.init_endpoint),
        id,
        "init_endpoint",
    )?;
    let sign_headers = req(
        raw.sign_headers.as_ref(),
        parent.map(|p| &p.sign_headers),
        id,
        "sign_headers",
    )?;

    let raw_caps = raw.caps.as_ref();
    let codecs = req(
        raw_caps.and_then(|c| c.codecs.as_ref()),
        parent.map(|p| &p.codecs),
        id,
        "caps.codecs",
    )?;
    let audio_codecs = req(
        raw_caps.and_then(|c| c.audio_codecs.as_ref()),
        parent.map(|p| &p.audio_codecs),
        id,
        "caps.audio_codecs",
    )?;
    let transports = req(
        raw_caps.and_then(|c| c.transports.as_ref()),
        parent.map(|p| &p.transports),
        id,
        "caps.transports",
    )?;
    let data_channels = req(
        raw_caps.and_then(|c| c.data_channels.as_ref()),
        parent.map(|p| &p.data_channels),
        id,
        "caps.data_channels",
    )?;

    validate_cap_list(id, "caps.codecs", &codecs, KNOWN_CODECS)?;
    validate_cap_list(id, "caps.audio_codecs", &audio_codecs, KNOWN_AUDIO_CODECS)?;
    validate_cap_list(id, "caps.transports", &transports, KNOWN_TRANSPORTS)?;
    validate_cap_list(
        id,
        "caps.data_channels",
        &data_channels,
        KNOWN_DATA_CHANNELS,
    )?;

    let rf = raw_caps.and_then(|c| c.features.as_ref());
    let pf = parent.map(|p| &p.features);
    let features = ResolvedFeatures {
        capture_setting: req(
            rf.and_then(|f| f.capture_setting.as_ref()),
            pf.map(|f| &f.capture_setting),
            id,
            "caps.features.capture_setting",
        )?,
        simple_action: req(
            rf.and_then(|f| f.simple_action.as_ref()),
            pf.map(|f| &f.simple_action),
            id,
            "caps.features.simple_action",
        )?,
        system_metrics: req(
            rf.and_then(|f| f.system_metrics.as_ref()),
            pf.map(|f| &f.system_metrics),
            id,
            "caps.features.system_metrics",
        )?,
        private_screen: req(
            rf.and_then(|f| f.private_screen.as_ref()),
            pf.map(|f| &f.private_screen),
            id,
            "caps.features.private_screen",
        )?,
        update_acquire: req(
            rf.and_then(|f| f.update_acquire.as_ref()),
            pf.map(|f| &f.update_acquire),
            id,
            "caps.features.update_acquire",
        )?,
        file_transfer_ftp: req(
            rf.and_then(|f| f.file_transfer_ftp.as_ref()),
            pf.map(|f| &f.file_transfer_ftp),
            id,
            "caps.features.file_transfer_ftp",
        )?,
        file_transfer_ftp2: req(
            rf.and_then(|f| f.file_transfer_ftp2.as_ref()),
            pf.map(|f| &f.file_transfer_ftp2),
            id,
            "caps.features.file_transfer_ftp2",
        )?,
        clipboard: req(
            rf.and_then(|f| f.clipboard.as_ref()),
            pf.map(|f| &f.clipboard),
            id,
            "caps.features.clipboard",
        )?,
        qos_stat: req(
            rf.and_then(|f| f.qos_stat.as_ref()),
            pf.map(|f| &f.qos_stat),
            id,
            "caps.features.qos_stat",
        )?,
        mumu_control: req(
            rf.and_then(|f| f.mumu_control.as_ref()),
            pf.map(|f| &f.mumu_control),
            id,
            "caps.features.mumu_control",
        )?,
        virtual_mouse_device: req(
            rf.and_then(|f| f.virtual_mouse_device.as_ref()),
            pf.map(|f| &f.virtual_mouse_device),
            id,
            "caps.features.virtual_mouse_device",
        )?,
    };

    let rq = raw.quirks.as_ref();
    let pq = parent.map(|p| &p.quirks);
    let quirks = ResolvedQuirks {
        kcp_quic_optional: req(
            rq.and_then(|q| q.kcp_quic_optional.as_ref()),
            pq.map(|q| &q.kcp_quic_optional),
            id,
            "quirks.kcp_quic_optional",
        )?,
        log_archive_push_only: req(
            rq.and_then(|q| q.log_archive_push_only.as_ref()),
            pq.map(|q| &q.log_archive_push_only),
            id,
            "quirks.log_archive_push_only",
        )?,
        wallpaper_raw_post: req(
            rq.and_then(|q| q.wallpaper_raw_post.as_ref()),
            pq.map(|q| &q.wallpaper_raw_post),
            id,
            "quirks.wallpaper_raw_post",
        )?,
    };

    let rs = raw.signal.as_ref();
    let ps = parent.map(|p| &p.signal);
    let def_sig = ResolvedSignal::default();
    let signal = ResolvedSignal {
        path: rs
            .and_then(|s| s.path.clone())
            .or_else(|| ps.map(|s| s.path.clone()))
            .unwrap_or(def_sig.path),
        handshake_timeout_ms: rs
            .and_then(|s| s.handshake_timeout_ms)
            .or_else(|| ps.map(|s| s.handshake_timeout_ms))
            .unwrap_or(def_sig.handshake_timeout_ms),
        ping_interval_ms: rs
            .and_then(|s| s.ping_interval_ms)
            .or_else(|| ps.map(|s| s.ping_interval_ms))
            .unwrap_or(def_sig.ping_interval_ms),
        ping_timeout_ms: rs
            .and_then(|s| s.ping_timeout_ms)
            .or_else(|| ps.map(|s| s.ping_timeout_ms))
            .unwrap_or(def_sig.ping_timeout_ms),
        reconnect_initial_ms: rs
            .and_then(|s| s.reconnect_initial_ms)
            .or_else(|| ps.map(|s| s.reconnect_initial_ms))
            .unwrap_or(def_sig.reconnect_initial_ms),
        reconnect_max_ms: rs
            .and_then(|s| s.reconnect_max_ms)
            .or_else(|| ps.map(|s| s.reconnect_max_ms))
            .unwrap_or(def_sig.reconnect_max_ms),
        reconnect_key_refresh_ms: rs
            .and_then(|s| s.reconnect_key_refresh_ms)
            .or_else(|| ps.map(|s| s.reconnect_key_refresh_ms))
            .unwrap_or(def_sig.reconnect_key_refresh_ms),
    };

    let detect_plat = raw
        .detect
        .as_ref()
        .and_then(|d| d.plat.clone())
        .unwrap_or_else(|| plat.clone());
    let detect_version = raw
        .detect
        .as_ref()
        .and_then(|d| d.version.clone())
        .unwrap_or_else(|| version.clone());
    let detect_build = match raw.detect.as_ref() {
        Some(d) => d.build,
        None => Some(build),
    };

    Ok(ResolvedProfile {
        id: raw.id.clone(),
        inherits: raw.inherits.clone(),
        source,
        evidence,
        platform,
        plat,
        version,
        build,
        bundle_id,
        channel,
        min_os,
        init_endpoint,
        sign_headers,
        detect_plat,
        detect_version,
        detect_build,
        codecs,
        audio_codecs,
        transports,
        data_channels,
        features,
        quirks,
        signal,
    })
}

fn map_platform(platform: &str, id: &str) -> Result<&'static str, String> {
    match platform {
        "windows" => Ok("Platform::Windows"),
        "macos" => Ok("Platform::MacOs"),
        "linux" => Ok("Platform::Linux"),
        "ios" => Ok("Platform::Ios"),
        "android" => Ok("Platform::Android"),
        other => Err(format!("unknown platform {other:?} in profile {id:?}")),
    }
}

fn validate_cap_list(
    id: &str,
    field: &str,
    items: &[String],
    allowed: &[&str],
) -> Result<(), String> {
    let mut seen = HashSet::new();
    for item in items {
        if !allowed.contains(&item.as_str()) {
            return Err(format!(
                "profile {id:?} has unknown capability {item:?} in {field}"
            ));
        }
        if !seen.insert(item.as_str()) {
            return Err(format!(
                "profile {id:?} has duplicate capability {item:?} in {field}"
            ));
        }
    }
    Ok(())
}

fn check_duplicate_detect_rules(profiles: &[ResolvedProfile]) -> Result<(), String> {
    for (i, a) in profiles.iter().enumerate() {
        for b in &profiles[i + 1..] {
            if a.detect_plat == b.detect_plat && a.detect_version == b.detect_version {
                let builds_overlap = match (a.detect_build, b.detect_build) {
                    (Some(ba), Some(bb)) => ba == bb,
                    (None, _) | (_, None) => true,
                };
                if builds_overlap {
                    return Err(format!(
                        "duplicate detect rule across profiles {:?} and {:?}: (plat={:?}, version={:?}, build={:?}/{:?})",
                        a.id, b.id, a.detect_plat, a.detect_version, a.detect_build, b.detect_build
                    ));
                }
            }
        }
    }
    Ok(())
}
