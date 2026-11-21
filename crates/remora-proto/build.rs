use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[path = "src/profile/resolver.rs"]
mod resolver;
#[allow(dead_code)]
#[path = "src/profile/types.rs"]
mod types;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protox compiles the schema in pure Rust, so building needs no system protoc.
    let descriptors = protox::compile(["gv_pb.proto"], ["proto"])?;
    prost_build::Config::new().compile_fds(descriptors)?;
    println!("cargo::rerun-if-changed=proto");

    emit_profiles(Path::new("profiles"))?;
    println!("cargo::rerun-if-changed=profiles");
    Ok(())
}

fn emit_profiles(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    let mut tables = Vec::with_capacity(paths.len());
    for path in paths {
        println!("cargo::rerun-if-changed={}", path.display());
        let raw = fs::read_to_string(&path)?;
        let table: toml::Table = toml::from_str(&raw)?;
        tables.push(table);
    }

    let profiles = resolver::resolve_all(tables)
        .map_err(|err| -> Box<dyn std::error::Error> { err.into() })?;
    let json = serde_json::to_vec(&profiles)?;

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR missing")?);
    fs::write(out_dir.join("profiles.json"), json)?;
    Ok(())
}
