use std::collections::{HashMap, HashSet};

use super::types::{Profile, ProfileSource};

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

pub fn resolve_all(inputs: Vec<toml::Table>) -> Result<Vec<Profile>, String> {
    let mut by_id = HashMap::new();
    let mut order = Vec::with_capacity(inputs.len());
    for raw in inputs {
        let id = raw
            .get("id")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| "profile missing string field \"id\"".to_owned())?
            .to_owned();
        if id.trim().is_empty() {
            return Err("profile id must not be empty".into());
        }
        if by_id.contains_key(&id) {
            return Err(format!("duplicate profile id {id:?}"));
        }
        order.push(id.clone());
        by_id.insert(id, raw);
    }

    let mut table_cache: HashMap<String, toml::Table> = HashMap::new();
    let mut resolved = Vec::with_capacity(order.len());
    for id in &order {
        let mut stack = Vec::new();
        let mut merged = resolve_table(id, &by_id, &mut table_cache, &mut stack)?;
        finalize_detect(&mut merged);

        let profile: Profile = toml::Value::Table(merged)
            .try_into()
            .map_err(|err| format!("profile {id:?}: {err}"))?;
        validate_profile(&profile)?;
        resolved.push(profile);
    }

    check_duplicate_detect_rules(&resolved)?;
    Ok(resolved)
}

fn resolve_table(
    id: &str,
    by_id: &HashMap<String, toml::Table>,
    cache: &mut HashMap<String, toml::Table>,
    stack: &mut Vec<String>,
) -> Result<toml::Table, String> {
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

    let inherits = match raw.get("inherits") {
        None => None,
        Some(toml::Value::String(parent_id)) => Some(parent_id.as_str()),
        Some(_) => return Err(format!("profile {id:?}: \"inherits\" must be a string")),
    };

    stack.push(id.to_owned());
    let merged = match inherits {
        Some(parent_id) => {
            let parent = resolve_table(parent_id, by_id, cache, stack)?;
            merge_tables(&parent, raw)
        }
        None => raw.clone(),
    };
    stack.pop();

    cache.insert(id.to_owned(), merged.clone());
    Ok(merged)
}

fn merge_tables(parent: &toml::Table, child: &toml::Table) -> toml::Table {
    let mut out = parent.clone();
    // `inherits` and `detect` describe the profile itself and never cascade to children.
    out.remove("inherits");
    out.remove("detect");
    merge_into(&mut out, child);
    out
}

fn merge_into(dst: &mut toml::Table, src: &toml::Table) {
    for (key, value) in src {
        match (dst.get_mut(key), value) {
            (Some(toml::Value::Table(dst_sub)), toml::Value::Table(src_sub)) => {
                merge_into(dst_sub, src_sub);
            }
            _ => {
                dst.insert(key.clone(), value.clone());
            }
        }
    }
}

fn finalize_detect(table: &mut toml::Table) {
    let plat = table.get("plat").cloned();
    let version = table.get("version").cloned();
    let build = table.get("build").cloned();

    match table.get_mut("detect") {
        None => {
            let mut dt = toml::Table::new();
            if let Some(v) = plat {
                dt.insert("plat".into(), v);
            }
            if let Some(v) = version {
                dt.insert("version".into(), v);
            }
            if let Some(v) = build {
                dt.insert("build".into(), v);
            }
            table.insert("detect".into(), toml::Value::Table(dt));
        }
        Some(toml::Value::Table(dt)) => {
            if !dt.contains_key("plat")
                && let Some(v) = plat
            {
                dt.insert("plat".into(), v);
            }
            if !dt.contains_key("version")
                && let Some(v) = version
            {
                dt.insert("version".into(), v);
            }
        }
        Some(_) => {}
    }
}

fn validate_profile(profile: &Profile) -> Result<(), String> {
    if matches!(
        profile.source,
        ProfileSource::ReverseEngineered | ProfileSource::Captured
    ) && (profile.evidence.is_empty() || profile.evidence.iter().any(|e| e.trim().is_empty()))
    {
        return Err(format!(
            "profile {:?} with source {:?} must have non-empty evidence",
            profile.id, profile.source
        ));
    }

    validate_cap_list(
        &profile.id,
        "caps.codecs",
        &profile.caps.codecs,
        KNOWN_CODECS,
    )?;
    validate_cap_list(
        &profile.id,
        "caps.audio_codecs",
        &profile.caps.audio_codecs,
        KNOWN_AUDIO_CODECS,
    )?;
    validate_cap_list(
        &profile.id,
        "caps.transports",
        &profile.caps.transports,
        KNOWN_TRANSPORTS,
    )?;
    validate_cap_list(
        &profile.id,
        "caps.data_channels",
        &profile.caps.data_channels,
        KNOWN_DATA_CHANNELS,
    )?;
    Ok(())
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

fn check_duplicate_detect_rules(profiles: &[Profile]) -> Result<(), String> {
    for (i, a) in profiles.iter().enumerate() {
        for b in &profiles[i + 1..] {
            if a.detect.plat == b.detect.plat && a.detect.version == b.detect.version {
                let builds_overlap = match (a.detect.build, b.detect.build) {
                    (Some(ba), Some(bb)) => ba == bb,
                    (None, _) | (_, None) => true,
                };
                if builds_overlap {
                    return Err(format!(
                        "duplicate detect rule across profiles {:?} and {:?}: (plat={:?}, version={:?}, build={:?}/{:?})",
                        a.id, b.id, a.detect.plat, a.detect.version, a.detect.build, b.detect.build
                    ));
                }
            }
        }
    }
    Ok(())
}
