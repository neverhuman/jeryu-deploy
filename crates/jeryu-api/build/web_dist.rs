//! Which SPA dist `build.rs` embeds, and the proof that it is the pinned one.
//!
//! `jeryu-split.lock.toml` pins jeryu-web by a 40-hex `commit` and the
//! `web_dist_sha256` of the dist that commit builds to. The hash is the sha256
//! of the dist's manifest: one `sha256sum`-format line (`<hex>  <path>`) per
//! regular file, sorted by path bytes — exactly what the release
//! `build-web-dist.sh` writes. This module is shared by `build.rs` and the
//! `web_dist_pin` integration test, so it uses only `sha2`, `toml` and `std`.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Pinned dist to verify against the lock and embed.
pub const WEB_DIST_ENV: &str = "JERYU_WEB_DIST";
/// Explicit, unverified local dist for tests and dev builds; refused in release.
pub const WEB_DIST_LOCAL_ENV: &str = "JERYU_WEB_DIST_LOCAL";

/// The jeryu-web entry of the split lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebPin {
    pub commit: String,
    pub dist_sha256: String,
}

/// What `build.rs` embeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebDistSource {
    /// A dist whose manifest hash matched the lock.
    Pinned { dir: PathBuf, pin: WebPin },
    /// An explicitly requested local dist (not verified; never in release).
    Local { dir: PathBuf },
    /// No dist: a non-release build that asked for none.
    Absent,
}

/// Parse and validate the jeryu-web pin; `web_artifact` must be `"pinned"`.
pub fn read_pin(lock_text: &str) -> Result<WebPin, String> {
    let lock: toml::Value =
        toml::from_str(lock_text).map_err(|err| format!("split lock is not TOML: {err}"))?;
    match lock.get("web_artifact").and_then(toml::Value::as_str) {
        Some("pinned") => {}
        other => {
            return Err(format!(
                "split lock web_artifact must be \"pinned\", found {other:?}"
            ));
        }
    }
    let web = lock
        .get("repo")
        .and_then(toml::Value::as_array)
        .and_then(|repos| {
            repos
                .iter()
                .find(|repo| repo.get("name").and_then(toml::Value::as_str) == Some("jeryu-web"))
        })
        .ok_or("split lock has no jeryu-web [[repo]] entry")?;
    let field = |key: &str, len: usize| -> Result<String, String> {
        let value = web
            .get(key)
            .and_then(toml::Value::as_str)
            .ok_or(format!("split lock jeryu-web entry has no {key}"))?;
        if value.len() != len
            || !value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(format!(
                "split lock jeryu-web {key} must be {len} lowercase hex digits, found {value:?}"
            ));
        }
        Ok(value.to_string())
    };
    Ok(WebPin {
        commit: field("commit", 40)?,
        dist_sha256: field("web_dist_sha256", 64)?,
    })
}

/// Every regular file under `root` as (slash-separated relative path, full path),
/// sorted by path bytes. Symlinks, special files and names `sha256sum` would
/// escape are refused.
pub fn dist_files(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
        let entries = fs::read_dir(dir).map_err(|err| format!("read {}: {err}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| format!("read {}: {err}", dir.display()))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|err| format!("stat {}: {err}", path.display()))?;
            if kind.is_dir() {
                walk(root, &path, out)?;
                continue;
            }
            if !kind.is_file() {
                return Err(format!(
                    "web dist entry is not a regular file: {}",
                    path.display()
                ));
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| format!("{} is outside the web dist", path.display()))?
                .to_str()
                .ok_or(format!("web dist path is not UTF-8: {}", path.display()))?
                .replace('\\', "/");
            if relative.contains('\n') || relative.contains('\r') {
                return Err(format!("web dist path has a line break: {relative:?}"));
            }
            out.push((relative, path));
        }
        Ok(())
    }
    if !root.is_dir() {
        return Err(format!("web dist {} is missing", root.display()));
    }
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(files)
}

/// The dist manifest, byte-identical to
/// `find . -type f -printf '%P\0' | LC_ALL=C sort -z | xargs -0 sha256sum`.
pub fn manifest(root: &Path) -> Result<String, String> {
    let files = dist_files(root)?;
    if !files.iter().any(|(path, _)| path == "index.html") {
        return Err(format!("web dist {} has no index.html", root.display()));
    }
    let mut out = String::new();
    for (relative, path) in files {
        let bytes = fs::read(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
        let _ = writeln!(out, "{}  {relative}", hex(&Sha256::digest(&bytes)));
    }
    Ok(out)
}

/// sha256 of [`manifest`]: the value pinned as `web_dist_sha256`.
pub fn dist_sha256(root: &Path) -> Result<String, String> {
    Ok(hex(&Sha256::digest(manifest(root)?.as_bytes())))
}

/// Decide what to embed. `pinned`/`local` are the two env vars, `release` is
/// whether this is a release-profile build. Fails closed: a pinned dist must
/// hash to the lock, a release build must have one, and nothing is implied.
pub fn resolve(
    lock_text: &str,
    pinned: Option<&Path>,
    local: Option<&Path>,
    release: bool,
) -> Result<WebDistSource, String> {
    match (pinned, local) {
        (Some(_), Some(_)) => Err(format!(
            "set only one of {WEB_DIST_ENV} (pinned) and {WEB_DIST_LOCAL_ENV} (local)"
        )),
        (Some(dir), None) => {
            let pin = read_pin(lock_text)?;
            let actual = dist_sha256(dir)?;
            if actual != pin.dist_sha256 {
                return Err(format!(
                    "web dist {} hashes to {actual}, but jeryu-split.lock.toml pins jeryu-web {} \
                     at {}; rebuild it with the release build-web-dist.sh",
                    dir.display(),
                    pin.commit,
                    pin.dist_sha256
                ));
            }
            Ok(WebDistSource::Pinned {
                dir: dir.to_path_buf(),
                pin,
            })
        }
        (None, Some(_)) if release => Err(format!(
            "{WEB_DIST_LOCAL_ENV} is for tests and dev builds; a release embeds the pinned \
             dist from {WEB_DIST_ENV}"
        )),
        (None, Some(dir)) => {
            dist_files(dir)?;
            Ok(WebDistSource::Local {
                dir: dir.to_path_buf(),
            })
        }
        (None, None) if release => Err(format!(
            "a release build needs {WEB_DIST_ENV}: the jeryu-web dist built by the release \
             build-web-dist.sh at the commit pinned in jeryu-split.lock.toml"
        )),
        (None, None) => {
            read_pin(lock_text)?;
            Ok(WebDistSource::Absent)
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
