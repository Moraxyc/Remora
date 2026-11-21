use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileToml {
    id: String,
    platform: String,
    plat: String,
    version: String,
    build: u32,
    bundle_id: String,
    channel: String,
    min_os: String,
    init_endpoint: String,
    sign_headers: Vec<String>,
    caps: CapsToml,
    quirks: QuirksToml,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapsToml {
    codecs: Vec<String>,
    audio_codecs: Vec<String>,
    transports: Vec<String>,
    data_channels: Vec<String>,
    features: FeaturesToml,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeaturesToml {
    capture_setting: i32,
    simple_action: i32,
    system_metrics: i32,
    private_screen: i32,
    update_acquire: i32,
    file_transfer_ftp: i32,
    file_transfer_ftp2: i32,
    clipboard: i32,
    qos_stat: i32,
    mumu_control: i32,
    virtual_mouse_device: i32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuirksToml {
    kcp_quic_optional: bool,
    log_archive_push_only: bool,
    wallpaper_raw_post: bool,
}

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

    let mut out = String::from("pub static PROFILES: &[Profile] = &[\n");
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
        let raw = fs::read_to_string(&path)?;
        let profile: ProfileToml = toml::from_str(&raw)?;
        let platform = match profile.platform.as_str() {
            "windows" => "Platform::Windows",
            "macos" => "Platform::MacOs",
            "linux" => "Platform::Linux",
            "ios" => "Platform::Ios",
            "android" => "Platform::Android",
            other => return Err(format!("unknown platform {other:?} in {}", path.display()).into()),
        };

        writeln!(out, "    Profile {{").unwrap();
        writeln!(out, "        id: {:?},", profile.id).unwrap();
        writeln!(out, "        platform: {platform},").unwrap();
        writeln!(out, "        plat: {:?},", profile.plat).unwrap();
        writeln!(out, "        version: {:?},", profile.version).unwrap();
        writeln!(out, "        build: {},", profile.build).unwrap();
        writeln!(out, "        bundle_id: {:?},", profile.bundle_id).unwrap();
        writeln!(out, "        channel: {:?},", profile.channel).unwrap();
        writeln!(out, "        min_os: {:?},", profile.min_os).unwrap();
        writeln!(out, "        init_endpoint: {:?},", profile.init_endpoint).unwrap();
        write_str_slice(&mut out, "        ", "sign_headers", &profile.sign_headers);
        writeln!(out, "        caps: Caps {{").unwrap();
        write_str_slice(&mut out, "            ", "codecs", &profile.caps.codecs);
        write_str_slice(
            &mut out,
            "            ",
            "audio_codecs",
            &profile.caps.audio_codecs,
        );
        write_str_slice(
            &mut out,
            "            ",
            "transports",
            &profile.caps.transports,
        );
        write_str_slice(
            &mut out,
            "            ",
            "data_channels",
            &profile.caps.data_channels,
        );
        writeln!(out, "            features: FeatureCaps {{").unwrap();
        let f = &profile.caps.features;
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
