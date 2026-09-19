//! Embed the jeryu-web SPA dist pinned in `jeryu-split.lock.toml`.
//!
//! The dist is never part of this repository. A release build gets it from
//! `JERYU_WEB_DIST` (built outside the checkout by the release
//! `build-web-dist.sh`) and refuses to build unless its manifest hash matches
//! the lock. Tests and dev builds may name an unverified dist with
//! `JERYU_WEB_DIST_LOCAL`, or embed none; both say so as a cargo warning.

#[path = "build/web_dist.rs"]
mod web_dist;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use web_dist::{WEB_DIST_ENV, WEB_DIST_LOCAL_ENV, WebDistSource};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let lock_path = manifest_dir.join("../../jeryu-split.lock.toml");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    println!("cargo:rerun-if-changed=build/web_dist.rs");
    println!("cargo:rerun-if-env-changed={WEB_DIST_ENV}");
    println!("cargo:rerun-if-env-changed={WEB_DIST_LOCAL_ENV}");

    let lock_text = fs::read_to_string(&lock_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", lock_path.display()));
    let dir_from = |name: &str| {
        env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let pinned = dir_from(WEB_DIST_ENV);
    let local = dir_from(WEB_DIST_LOCAL_ENV);
    let release = env::var("PROFILE").as_deref() == Ok("release");
    let source = web_dist::resolve(&lock_text, pinned.as_deref(), local.as_deref(), release)
        .unwrap_or_else(|err| panic!("web dist: {err}"));

    let dist = match &source {
        WebDistSource::Pinned { dir, .. } => Some(dir.clone()),
        WebDistSource::Local { dir } => {
            println!(
                "cargo:warning=embedding the unverified local web dist {} ({WEB_DIST_LOCAL_ENV})",
                dir.display()
            );
            Some(dir.clone())
        }
        WebDistSource::Absent => {
            println!(
                "cargo:warning=no web dist embedded: set {WEB_DIST_ENV} to the pinned jeryu-web \
                 dist (required for release builds) or {WEB_DIST_LOCAL_ENV} to a local one"
            );
            None
        }
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let out = out_dir.join("embedded_web.rs");
    let mut assets = Vec::new();

    if let Some(dist) = &dist {
        println!("cargo:rerun-if-changed={}", dist.display());
        collect_assets(dist, dist, &mut assets);
    }

    assets.sort_by(|left, right| left.0.cmp(&right.0));
    let mut generated = String::from("pub(crate) static ASSETS: &[EmbeddedAsset] = &[\n");
    for (route_path, file_path) in assets {
        println!("cargo:rerun-if-changed={}", file_path.display());
        generated.push_str("    EmbeddedAsset {\n");
        generated.push_str(&format!("        path: {:?},\n", route_path));
        generated.push_str(&format!(
            "        content_type: {:?},\n",
            content_type(&route_path)
        ));
        generated.push_str(&format!(
            "        bytes: include_bytes!({:?}),\n",
            file_path.display().to_string()
        ));
        generated.push_str("    },\n");
    }
    generated.push_str("];\n");

    fs::write(out, generated).expect("write embedded web asset table");
}

fn collect_assets(root: &Path, dir: &Path, assets: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).expect("read web dist directory") {
        let entry = entry.expect("read web dist entry");
        let path = entry.path();
        if path.is_dir() {
            collect_assets(root, &path, assets);
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("asset path under web dist")
            .to_string_lossy()
            .replace('\\', "/");
        assets.push((relative, path));
    }
}

fn content_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|part| part.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
