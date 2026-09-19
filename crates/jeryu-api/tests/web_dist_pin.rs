//! The SPA dist `build.rs` embeds is the one pinned in jeryu-split.lock.toml:
//! a tampered, stale or missing dist fails the build, and an unverified local
//! dist is only ever an explicit, non-release choice.

#[path = "../build/web_dist.rs"]
#[allow(dead_code)]
mod web_dist;

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::tempdir;
use web_dist::{WebDistSource, dist_sha256, manifest, read_pin, resolve};

const COMMIT: &str = "cdbef2cbf2fb93ca41ff780fc56f91d0b16620c2";

fn lock_for(hash: &str) -> String {
    format!(
        r#"
schema_version = "jeryu.split.lock/v1"
web_artifact = "pinned"

[[repo]]
name = "jeryu-core"
commit = "4e00f9b076ffbb0fd02fc7dcda4e64c2fd57bf2f"

[[repo]]
name = "jeryu-web"
commit = "{COMMIT}"
web_dist_sha256 = "{hash}"
"#
    )
}

fn write_dist(root: &Path) {
    fs::create_dir_all(root.join("assets")).unwrap();
    fs::write(
        root.join("index.html"),
        "<!doctype html><script src=\"/assets/app-1.js\"></script>",
    )
    .unwrap();
    fs::write(root.join("assets/app-1.js"), "console.log(1);\n").unwrap();
    fs::write(root.join("assets/Z.css"), "a{}\n").unwrap();
}

#[test]
fn manifest_matches_sha256sum_over_sorted_paths() {
    let dir = tempdir().unwrap();
    write_dist(dir.path());
    let ours = manifest(dir.path()).unwrap();
    let paths: Vec<&str> = ours.lines().map(|line| &line[66..]).collect();
    assert_eq!(paths, ["assets/Z.css", "assets/app-1.js", "index.html"]);

    // The release build-web-dist.sh computes it with coreutils; both must agree.
    let shell = Command::new("bash")
        .arg("-c")
        .arg("find . -type f -printf '%P\\0' | LC_ALL=C sort -z | xargs -0 sha256sum")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(shell.status.success());
    assert_eq!(String::from_utf8(shell.stdout).unwrap(), ours);
}

#[test]
fn a_pinned_dist_that_matches_the_lock_is_embedded() {
    let dir = tempdir().unwrap();
    write_dist(dir.path());
    let lock = lock_for(&dist_sha256(dir.path()).unwrap());
    let source = resolve(&lock, Some(dir.path()), None, true).unwrap();
    match source {
        WebDistSource::Pinned { dir: embedded, pin } => {
            assert_eq!(embedded, dir.path());
            assert_eq!(pin.commit, COMMIT);
        }
        other => panic!("expected the pinned dist, got {other:?}"),
    }
}

#[test]
fn a_tampered_dist_fails_the_build() {
    let dir = tempdir().unwrap();
    write_dist(dir.path());
    let lock = lock_for(&dist_sha256(dir.path()).unwrap());
    fs::write(dir.path().join("assets/app-1.js"), "console.log(2);\n").unwrap();
    let err = resolve(&lock, Some(dir.path()), None, true).unwrap_err();
    assert!(
        err.contains("but jeryu-split.lock.toml pins jeryu-web"),
        "{err}"
    );

    // An extra file changes the manifest too.
    let dir = tempdir().unwrap();
    write_dist(dir.path());
    let lock = lock_for(&dist_sha256(dir.path()).unwrap());
    fs::write(dir.path().join("extra.js"), "x").unwrap();
    assert!(resolve(&lock, Some(dir.path()), None, false).is_err());
}

#[test]
fn a_stale_dist_fails_the_build() {
    let dir = tempdir().unwrap();
    write_dist(dir.path());
    let other = "0".repeat(64);
    let err = resolve(&lock_for(&other), Some(dir.path()), None, false).unwrap_err();
    assert!(err.contains(&other), "{err}");
}

#[test]
fn a_missing_dist_fails_the_build() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("dist");
    let err = resolve(&lock_for(&"0".repeat(64)), Some(&missing), None, true).unwrap_err();
    assert!(err.contains("is missing"), "{err}");

    fs::create_dir_all(&missing).unwrap();
    let err = resolve(&lock_for(&"0".repeat(64)), Some(&missing), None, true).unwrap_err();
    assert!(err.contains("no index.html"), "{err}");
}

#[test]
fn a_release_build_requires_the_pinned_dist() {
    let lock = lock_for(&"0".repeat(64));
    let err = resolve(&lock, None, None, true).unwrap_err();
    assert!(
        err.contains("a release build needs JERYU_WEB_DIST"),
        "{err}"
    );

    let dir = tempdir().unwrap();
    write_dist(dir.path());
    let err = resolve(&lock, None, Some(dir.path()), true).unwrap_err();
    assert!(
        err.contains("JERYU_WEB_DIST_LOCAL is for tests and dev builds"),
        "{err}"
    );
    assert!(resolve(&lock, Some(dir.path()), Some(dir.path()), false).is_err());
}

#[test]
fn dev_builds_take_a_local_dist_only_when_asked() {
    let lock = lock_for(&"0".repeat(64));
    let dir = tempdir().unwrap();
    write_dist(dir.path());
    assert_eq!(
        resolve(&lock, None, Some(dir.path()), false).unwrap(),
        WebDistSource::Local {
            dir: dir.path().to_path_buf()
        }
    );
    assert_eq!(
        resolve(&lock, None, None, false).unwrap(),
        WebDistSource::Absent
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_in_the_dist_is_refused() {
    let dir = tempdir().unwrap();
    write_dist(dir.path());
    std::os::unix::fs::symlink("/etc/hostname", dir.path().join("host")).unwrap();
    let err = dist_sha256(dir.path()).unwrap_err();
    assert!(err.contains("not a regular file"), "{err}");
}

#[test]
fn the_lock_must_pin_jeryu_web() {
    assert!(
        read_pin(&lock_for(&"0".repeat(64)).replace("\"pinned\"", "\"local-or-pinned\"")).is_err()
    );
    assert!(read_pin(&lock_for("abc")).is_err());
    assert!(read_pin(&lock_for(&"0".repeat(64)).replace(COMMIT, "68bdc86")).is_err());

    let repo_lock = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../jeryu-split.lock.toml"
    ))
    .unwrap();
    let pin = read_pin(&repo_lock).unwrap();
    assert_eq!(pin.commit.len(), 40);
}
