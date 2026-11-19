//! Finds the web app's build, which the binary embeds: `REMORA_WEB_DIR`, or `web/build` in the
//! repository, made by `npm run build` there.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-env-changed=REMORA_WEB_DIR");
    let dir = match env::var_os("REMORA_WEB_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"))
            .join("../../web/build"),
    };
    let Ok(dir) = dir.join("app.html").canonicalize().map(|page| {
        page.parent()
            .expect("a file has a parent directory")
            .to_owned()
    }) else {
        panic!(
            "no web app build at {}: run `npm ci && npm run build` in web/, or point \
             REMORA_WEB_DIR at a build",
            dir.display()
        );
    };
    println!("cargo::rerun-if-changed={}", dir.display());
    println!("cargo::rustc-env=REMORA_WEB_DIR={}", dir.display());
}
