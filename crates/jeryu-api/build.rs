//! Embed the jeryu-web SPA dist pinned in `jeryu-split.lock.toml`.
//!
//! The dist is never part of this repository. A release build gets it from
//! `JERYU_WEB_DIST` (built outside the checkout by the release
//! `build-web-dist.sh`) and refuses to build unless its manifest hash matches
//! the lock. Tests and dev builds may name an unverified dist with
//! `JERYU_WEB_DIST_LOCAL`, or embed none; both say so as a cargo warning.
//!
//! It also stamps the build's identity for `/api/v1/version` and `/runners`:
//! `JERYU_BUILD_COMMIT` (this repository's commit: the env var of that name if
//! set, else `git rev-parse HEAD`, else nothing) and `JERYU_WEB_COMMIT` (the
//! jeryu-web commit the split lock pins).

#[path = "build/web_dist.rs"]
mod web_dist;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    emit_build_identity(&manifest_dir, &lock_text);
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

/// Explicit build commit, for builds whose checkout has no usable `.git`.
const BUILD_COMMIT_ENV: &str = "JERYU_BUILD_COMMIT";

fn emit_build_identity(manifest_dir: &Path, lock_text: &str) {
    println!("cargo:rerun-if-env-changed={BUILD_COMMIT_ENV}");
    let explicit = env::var(BUILD_COMMIT_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let commit = match explicit {
        Some(commit) => {
            assert!(
                is_commit(&commit),
                "{BUILD_COMMIT_ENV} must be 7-64 lowercase hex digits, found {commit:?}"
            );
            Some(commit)
        }
        None => git_head(manifest_dir),
    };
    match commit {
        Some(commit) => println!("cargo:rustc-env={BUILD_COMMIT_ENV}={commit}"),
        None => println!(
            "cargo:warning=build commit unknown: no {BUILD_COMMIT_ENV} and no git checkout; \
             /api/v1/version reports commit null"
        ),
    }
    // A malformed lock is refused (with its own message) by web_dist::resolve.
    if let Ok(pin) = web_dist::read_pin(lock_text) {
        println!("cargo:rustc-env=JERYU_WEB_COMMIT={}", pin.commit);
    }
}

fn is_commit(value: &str) -> bool {
    (7..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `git rev-parse HEAD` of the checkout holding this crate, and a rerun on the
/// files that move when HEAD does. Works in a linked worktree (where `.git` is
/// a file) and quietly gives `None` where there is no git or no repository.
fn git_head(dir: &Path) -> Option<String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()?;
        out.status.success().then_some(())?;
        let text = String::from_utf8(out.stdout).ok()?;
        Some(text.trim().to_string()).filter(|text| !text.is_empty())
    };
    let head = git(&["rev-parse", "HEAD"]).filter(|head| is_commit(head))?;
    let git_path = |name: &str| {
        git(&["rev-parse", "--path-format=absolute", "--git-path", name]).map(PathBuf::from)
    };
    let mut watch = vec![git_path("HEAD"), git_path("packed-refs")];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch.push(git_path(&branch));
    }
    // Only existing files: a missing one would rerun this script on every build.
    for path in watch.into_iter().flatten().filter(|path| path.is_file()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    Some(head)
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
