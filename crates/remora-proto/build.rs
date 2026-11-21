use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

#[path = "src/profile/resolver.rs"]
mod resolver;

use resolver::ProfileToml;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protox compiles the schema in pure Rust, so building needs no system protoc.
    let descriptors = protox::compile(["gv_pb.proto"], ["proto"])?;
    prost_build::Config::new().compile_fds(descriptors)?;
    println!("cargo:rerun-if-changed=proto");

    emit_profiles(Path::new("profiles"))?;
    println!("cargo:rerun-if-changed=profiles");
    Ok(())
}

fn emit_profiles(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    let mut raw_profiles = Vec::with_capacity(paths.len());
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
        let raw = fs::read_to_string(&path)?;
        let parsed: ProfileToml = toml::from_str(&raw)?;
        raw_profiles.push(parsed);
    }

    let profiles = resolver::resolve_all(raw_profiles)
        .map_err(|err| -> Box<dyn std::error::Error> { err.into() })?;

    let mut out = String::from("pub static PROFILES: &[Profile] = &[\n");
    for profile in profiles {
        writeln!(out, "    Profile {{").unwrap();
        writeln!(out, "        id: {:?},", profile.id).unwrap();
        match &profile.inherits {
            Some(parent) => writeln!(out, "        inherits: Some({parent:?}),").unwrap(),
            None => writeln!(out, "        inherits: None,").unwrap(),
        }
        writeln!(out, "        source: {},", profile.source).unwrap();
        write_str_slice(&mut out, "        ", "evidence", &profile.evidence);
        writeln!(out, "        platform: {},", profile.platform).unwrap();
        writeln!(out, "        plat: {:?},", profile.plat).unwrap();
        writeln!(out, "        version: {:?},", profile.version).unwrap();
        writeln!(out, "        build: {},", profile.build).unwrap();
        writeln!(out, "        bundle_id: {:?},", profile.bundle_id).unwrap();
        writeln!(out, "        channel: {:?},", profile.channel).unwrap();
        writeln!(out, "        min_os: {:?},", profile.min_os).unwrap();
        writeln!(out, "        init_endpoint: {:?},", profile.init_endpoint).unwrap();
        write_str_slice(&mut out, "        ", "sign_headers", &profile.sign_headers);
        writeln!(out, "        detect: DetectRule {{").unwrap();
        writeln!(out, "            plat: {:?},", profile.detect_plat).unwrap();
        writeln!(out, "            version: {:?},", profile.detect_version).unwrap();
        match profile.detect_build {
            Some(b) => writeln!(out, "            build: Some({b}),").unwrap(),
            None => writeln!(out, "            build: None,").unwrap(),
        }
        writeln!(out, "        }},").unwrap();
        writeln!(out, "        caps: Caps {{").unwrap();
        write_str_slice(&mut out, "            ", "codecs", &profile.codecs);
        write_str_slice(
            &mut out,
            "            ",
            "audio_codecs",
            &profile.audio_codecs,
        );
        write_str_slice(&mut out, "            ", "transports", &profile.transports);
        write_str_slice(
            &mut out,
            "            ",
            "data_channels",
            &profile.data_channels,
        );
        writeln!(out, "            features: FeatureCaps {{").unwrap();
        let f = &profile.features;
        writeln!(
            out,
            "                capture_setting: {},",
            f.capture_setting
        )
        .unwrap();
        writeln!(out, "                simple_action: {},", f.simple_action).unwrap();
        writeln!(out, "                system_metrics: {},", f.system_metrics).unwrap();
        writeln!(out, "                private_screen: {},", f.private_screen).unwrap();
        writeln!(out, "                update_acquire: {},", f.update_acquire).unwrap();
        writeln!(
            out,
            "                file_transfer_ftp: {},",
            f.file_transfer_ftp
        )
        .unwrap();
        writeln!(
            out,
            "                file_transfer_ftp2: {},",
            f.file_transfer_ftp2
        )
        .unwrap();
        writeln!(out, "                clipboard: {},", f.clipboard).unwrap();
        writeln!(out, "                qos_stat: {},", f.qos_stat).unwrap();
        writeln!(out, "                mumu_control: {},", f.mumu_control).unwrap();
        writeln!(
            out,
            "                virtual_mouse_device: {},",
            f.virtual_mouse_device
        )
        .unwrap();
        writeln!(out, "            }},").unwrap();
        writeln!(out, "        }},").unwrap();
        writeln!(out, "        quirks: Quirks {{").unwrap();
        writeln!(
            out,
            "            kcp_quic_optional: {},",
            profile.quirks.kcp_quic_optional
        )
        .unwrap();
        writeln!(
            out,
            "            log_archive_push_only: {},",
            profile.quirks.log_archive_push_only
        )
        .unwrap();
        writeln!(
            out,
            "            wallpaper_raw_post: {},",
            profile.quirks.wallpaper_raw_post
        )
        .unwrap();
        writeln!(out, "        }},").unwrap();
        writeln!(out, "    }},").unwrap();
    }
    out.push_str("];\n");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR missing")?);
    fs::write(out_dir.join("profiles.rs"), out)?;
    Ok(())
}

fn write_str_slice(out: &mut String, indent: &str, field: &str, items: &[String]) {
    writeln!(out, "{indent}{field}: &[").unwrap();
    for item in items {
        writeln!(out, "{indent}    {item:?},").unwrap();
    }
    writeln!(out, "{indent}],").unwrap();
}
