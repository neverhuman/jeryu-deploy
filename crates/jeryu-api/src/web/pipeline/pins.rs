//! Pins: `GET /api/v1/pins`, "what could be released".
//!
//! A deploy repo pins its dependencies: the web app by commit in its
//! `*-split.lock.toml`, Rust crates by git tag in its Cargo manifests. When a
//! dependency's default branch moves, nothing downstream said so: merged work
//! sat unpinned until somebody remembered to bump. This reads the pins of
//! every consumer straight from the hosted bare repositories and says how far
//! behind each one is, what a bump would ship, and whether a bump is already
//! open as a pull request.
//!
//! A consumer is any hosted repository whose default branch has a root
//! `*-split.lock.toml`. Two kinds of pin are reported:
//!
//! - `commit`: a lock `[[repo]]` entry that carries a `web_dist_sha256`. It is
//!   the only lock entry the release build really uses; the lock's tag and
//!   commit fields for Rust crates are not read by cargo and drift, so they
//!   are never reported.
//! - `tag` (or a `rev`, reported as `commit`): a git dependency in the
//!   consumer's root or `crates/*/Cargo.toml`. The dependency is the hosted
//!   repository named by the URL's last segment, whatever host the URL names.
//!
//! Lock shapes this does not know (a jain-style `repo = "..."` entry, a
//! `commit = "PENDING"`) are skipped. The answer is cached for a minute and
//! every git call is bounded, because the inbox polls this.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use jeryu_core::{CommitStatusState, PullRequestState, Repository};
use serde::Serialize;

use super::super::WebState;
use super::super::shift::{resolve_commit, run_git as git};
use crate::github::pulls::pull_request_web_path;

pub(crate) const PINS_SCHEMA: &str = "jeryu.pins/v1";
const CACHE_FOR: Duration = Duration::from_secs(60);
/// `behind` stops counting here; a pin this stale needs no finer number.
const MAX_BEHIND: &str = "1000";
const MAX_UNRELEASED: &str = "20";
const MAX_MANIFESTS: usize = 64;
const LOCK_SUFFIX: &str = "-split.lock.toml";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct BumpPr {
    pub number: u64,
    pub state: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Unreleased {
    pub sha: String,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Pin {
    /// `owner/name` of the hosted dependency.
    pub dependency: String,
    /// `commit` or `tag`.
    pub kind: &'static str,
    /// The file that holds the pin.
    pub source: String,
    /// The sha or tag as written.
    pub pinned_ref: String,
    pub pinned_sha: Option<String>,
    pub latest_sha: Option<String>,
    /// When the dependency's default branch last moved.
    pub latest_at: Option<String>,
    /// Commits on the default branch the pin does not reach.
    pub behind: u64,
    pub latest_green: Option<bool>,
    /// `current`, `behind`, `behind_not_green`, `diverged` or `unknown`.
    pub state: &'static str,
    pub bump_pr: Option<BumpPr>,
    /// What a bump would ship, newest first.
    pub unreleased: Vec<Unreleased>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Consumer {
    pub repo: String,
    pub family: Option<String>,
    pub branch: String,
    pub pins: Vec<Pin>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct PinsResponse {
    pub schema_version: &'static str,
    pub generated_at: String,
    pub consumers: Vec<Consumer>,
}

/// A pin as its file states it, before anything is resolved.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RawPin {
    pub name: String,
    pub kind: &'static str,
    pub pinned_ref: String,
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The lock entries a release build really uses: `name`, a 40-hex `commit`
/// and a `web_dist_sha256`. Anything else in the file is not a pin.
pub(crate) fn lock_pins(text: &str) -> Vec<RawPin> {
    let Ok(lock) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let entries = lock.get("repo").and_then(|repos| repos.as_array());
    entries
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?;
            let commit = entry.get("commit")?.as_str()?;
            let dist = entry.get("web_dist_sha256")?.as_str()?;
            (is_hex(commit, 40) && is_hex(dist, 64)).then(|| RawPin {
                name: name.to_string(),
                kind: "commit",
                pinned_ref: commit.to_string(),
            })
        })
        .collect()
}

/// `https://host/any/path/<name>.git` -> `<name>`.
fn dependency_name(url: &str) -> Option<String> {
    let last = url.trim_end_matches('/').rsplit('/').next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    let plain = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    plain.then(|| name.to_string())
}

fn table_pins(dependencies: &toml::Table, found: &mut BTreeSet<RawPin>) {
    for spec in dependencies.values() {
        let Some(spec) = spec.as_table() else {
            continue;
        };
        let Some(name) = spec
            .get("git")
            .and_then(|git| git.as_str())
            .and_then(dependency_name)
        else {
            continue;
        };
        let pin = match (
            spec.get("tag").and_then(|tag| tag.as_str()),
            spec.get("rev").and_then(|rev| rev.as_str()),
        ) {
            (Some(tag), _) => ("tag", tag),
            (None, Some(rev)) => ("commit", rev),
            (None, None) => continue,
        };
        found.insert(RawPin {
            name,
            kind: pin.0,
            pinned_ref: pin.1.to_string(),
        });
    }
}

/// Git dependencies pinned by `tag` or `rev` in one Cargo manifest.
pub(crate) fn manifest_pins(text: &str) -> BTreeSet<RawPin> {
    let mut found = BTreeSet::new();
    let Ok(manifest) = text.parse::<toml::Table>() else {
        return found;
    };
    let sections = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut tables: Vec<&toml::Table> = vec![&manifest];
    tables.extend(manifest.get("workspace").and_then(|w| w.as_table()));
    let targets = manifest.get("target").and_then(|t| t.as_table());
    tables.extend(
        targets
            .into_iter()
            .flat_map(|targets| targets.values())
            .filter_map(|target| target.as_table()),
    );
    for table in tables {
        for section in sections {
            if let Some(dependencies) = table.get(section).and_then(|d| d.as_table()) {
                table_pins(dependencies, &mut found);
            }
        }
    }
    found
}

/// What a pin's numbers mean.
pub(crate) fn classify(
    resolved: bool,
    diverged: bool,
    behind: u64,
    latest_green: Option<bool>,
) -> &'static str {
    match (resolved, diverged, behind, latest_green) {
        (false, ..) => "unknown",
        (true, true, ..) => "diverged",
        (true, false, 0, _) => "current",
        (true, false, _, Some(false)) => "behind_not_green",
        (true, false, ..) => "behind",
    }
}

fn text_of(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_string()
}

fn show_file(git_bin: &str, dir: &Path, branch: &str, path: &str) -> Option<String> {
    let spec = format!("refs/heads/{branch}:{path}");
    git(git_bin, dir, &["cat-file", "blob", &spec], &[], None)
        .ok()
        .map(|out| String::from_utf8_lossy(&out).into_owned())
}

fn list_files(git_bin: &str, dir: &Path, branch: &str, recursive: bool) -> Vec<String> {
    let tree = format!("refs/heads/{branch}");
    let mut args = vec!["ls-tree", "--name-only"];
    if recursive {
        args.push("-r");
    }
    args.push(&tree);
    git(git_bin, dir, &args, &[], None)
        .map(|out| text_of(out).lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The root manifest and one per crate: `Cargo.toml`, `crates/<name>/Cargo.toml`.
fn is_manifest(path: &str) -> bool {
    path == "Cargo.toml"
        || path
            .strip_prefix("crates/")
            .and_then(|rest| rest.strip_suffix("/Cargo.toml"))
            .is_some_and(|name| !name.contains('/'))
}

/// The hosted repository a name refers to: under the asking repository's owner
/// first, else the only hosted repository with that name. Shared with the
/// tool-finder, which resolves split-family members the same way.
pub(crate) fn hosted_repository<'a>(
    repos: &'a [Repository],
    owner: &str,
    name: &str,
) -> Option<&'a Repository> {
    let mut named = repos.iter().filter(|repo| repo.name == name);
    let same_owner = named.clone().find(|repo| repo.owner == owner);
    same_owner.or_else(|| match (named.next(), named.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    })
}

fn bump_pr(state: &WebState, consumer: &Repository, name: &str) -> Option<BumpPr> {
    let title_prefix = format!("release: pin {name}");
    let listed = state
        .core
        .list_pull_requests(&consumer.owner, &consumer.name, None)
        .ok()?;
    listed
        .into_iter()
        .filter(|pr| {
            !matches!(
                pr.state,
                PullRequestState::Closed | PullRequestState::Merged
            )
        })
        .find(|pr| {
            pr.title.starts_with(&title_prefix)
                || (pr.head.ref_name.starts_with("auto/pin-") && pr.title.contains(name))
        })
        .map(|pr| BumpPr {
            number: pr.number,
            state: serde_json::to_value(&pr.state)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "open".to_string()),
            url: pull_request_web_path(&pr.owner, &pr.repo, pr.number),
        })
}

fn latest_green(state: &WebState, dependency: &Repository, sha: &str) -> Option<bool> {
    let combined = state
        .core
        .combined_status(&dependency.owner, &dependency.name, sha)
        .ok()?;
    (combined.total_count > 0).then_some(combined.state == CommitStatusState::Success)
}

fn resolve_pin(
    state: &WebState,
    consumer: &Repository,
    dependency: &Repository,
    source: &str,
    raw: &RawPin,
) -> Pin {
    let git_bin = &state.repo_manager.config().git_bin;
    let mut pin = Pin {
        dependency: dependency.full_name.clone(),
        kind: raw.kind,
        source: source.to_string(),
        pinned_ref: raw.pinned_ref.clone(),
        pinned_sha: None,
        latest_sha: None,
        latest_at: None,
        behind: 0,
        latest_green: None,
        state: "unknown",
        bump_pr: None,
        unreleased: Vec::new(),
    };
    let Ok(opened) = state
        .repo_manager
        .open_parts(&dependency.owner, &dependency.name)
    else {
        return pin;
    };
    let dir = opened.path.as_path();
    let pinned_rev = match raw.kind {
        "tag" => format!("refs/tags/{}", raw.pinned_ref),
        _ => raw.pinned_ref.clone(),
    };
    pin.pinned_sha = resolve_commit(git_bin, dir, &pinned_rev);
    let head = format!("refs/heads/{}", dependency.default_branch);
    pin.latest_sha = resolve_commit(git_bin, dir, &head);
    let (Some(pinned), Some(latest)) = (pin.pinned_sha.clone(), pin.latest_sha.clone()) else {
        return pin;
    };
    pin.latest_green = latest_green(state, dependency, &latest);
    pin.latest_at = git(
        git_bin,
        dir,
        &["log", "-1", "--format=%cI", &latest],
        &[],
        None,
    )
    .ok()
    .map(text_of)
    .filter(|at| !at.is_empty());
    let diverged = git(
        git_bin,
        dir,
        &["merge-base", "--is-ancestor", &pinned, &latest],
        &[],
        None,
    )
    .is_err();
    let range = format!("{pinned}..{latest}");
    if !diverged && pinned != latest {
        pin.behind = git(
            git_bin,
            dir,
            &["rev-list", "--count", "--max-count", MAX_BEHIND, &range],
            &[],
            None,
        )
        .ok()
        .and_then(|out| text_of(out).parse().ok())
        .unwrap_or(0);
        let log = git(
            git_bin,
            dir,
            &["log", "-n", MAX_UNRELEASED, "--format=%H%x09%s", &range],
            &[],
            None,
        );
        pin.unreleased = log
            .map(text_of)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(sha, subject)| Unreleased {
                sha: sha.to_string(),
                subject: subject.to_string(),
            })
            .collect();
    }
    pin.state = classify(true, diverged, pin.behind, pin.latest_green);
    if pin.state != "current" {
        pin.bump_pr = bump_pr(state, consumer, &dependency.name);
    }
    pin
}

fn consumer_pins(
    state: &WebState,
    repos: &[Repository],
    consumer: &Repository,
) -> Option<Consumer> {
    let git_bin = &state.repo_manager.config().git_bin;
    let opened = state
        .repo_manager
        .open_parts(&consumer.owner, &consumer.name)
        .ok()?;
    let dir = opened.path.as_path();
    let branch = &consumer.default_branch;
    let root = list_files(git_bin, dir, branch, false);
    let locks: Vec<&String> = root
        .iter()
        .filter(|path| path.ends_with(LOCK_SUFFIX))
        .collect();
    if locks.is_empty() {
        return None;
    }
    let mut raw: Vec<(String, RawPin)> = Vec::new();
    for lock in locks {
        let text = show_file(git_bin, dir, branch, lock).unwrap_or_default();
        raw.extend(lock_pins(&text).into_iter().map(|pin| (lock.clone(), pin)));
    }
    let manifests = list_files(git_bin, dir, branch, true)
        .into_iter()
        .filter(|path| is_manifest(path))
        .take(MAX_MANIFESTS);
    let mut seen = BTreeSet::new();
    for manifest in manifests {
        let text = show_file(git_bin, dir, branch, &manifest).unwrap_or_default();
        for pin in manifest_pins(&text) {
            if seen.insert(pin.clone()) {
                raw.push((manifest.clone(), pin));
            }
        }
    }
    let pins = raw
        .iter()
        .filter(|(_, pin)| pin.name != consumer.name)
        .filter_map(|(source, pin)| {
            let dependency = hosted_repository(repos, &consumer.owner, &pin.name)?;
            Some(resolve_pin(state, consumer, dependency, source, pin))
        })
        .collect();
    let (family, _) =
        super::super::shift::shift_context(state, &consumer.owner, &consumer.name, "");
    Some(Consumer {
        repo: consumer.full_name.clone(),
        family,
        branch: branch.clone(),
        pins,
    })
}

/// Every consumer's pins, read from the hosted repositories right now.
pub(crate) fn collect(state: &WebState) -> PinsResponse {
    let repos = state.core.list_repositories(None);
    let consumers = repos
        .iter()
        .filter(|repo| !repo.archived && !repo.disabled)
        .filter_map(|consumer| consumer_pins(state, &repos, consumer))
        .collect();
    PinsResponse {
        schema_version: PINS_SCHEMA,
        generated_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        consumers,
    }
}

/// The last answer, reused for [`CACHE_FOR`]: the inbox and the Unreleased
/// page both poll this, and the answer only moves when a branch does.
#[derive(Clone, Default)]
pub(crate) struct PinsCache {
    inner: Arc<Mutex<Option<(Instant, PinsResponse)>>>,
}

/// The cached answer, or a fresh one. Runs git: call it off the async workers.
pub(crate) fn snapshot(state: &WebState) -> PinsResponse {
    let lock = || state.pins.inner.lock().expect("pins cache mutex poisoned");
    let cached = lock()
        .as_ref()
        .filter(|(at, _)| at.elapsed() < CACHE_FOR)
        .map(|(_, response)| response.clone());
    if let Some(response) = cached {
        return response;
    }
    let response = collect(state);
    *lock() = Some((Instant::now(), response.clone()));
    response
}

/// `GET /api/v1/pins` (admin-only by path, see `auth::admin_only_request`).
pub(crate) async fn pins(State(state): State<Arc<WebState>>) -> AxumResponse {
    let worker_state = state.clone();
    match tokio::task::spawn_blocking(move || snapshot(&worker_state)).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => {
            use super::super::workcells_support::{TypedError, typed_error};
            let reason = error.to_string();
            typed_error(TypedError {
                status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                code: "pins_collect_failed",
                purpose: "list what each deploy repo pins and what a bump would release",
                reason: &reason,
                common_fixes: &["retry in a few seconds"],
                docs_url: "docs/pipeline-events.md",
                repair_hint: "retry; if it persists check the server log for a collector panic",
                message: &reason,
            })
        }
    }
}
