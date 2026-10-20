//! What a pull request head is EXPECTED to be checked by, next to what was
//! actually reported on it.
//!
//! A checks tab built only from reported statuses says nothing at all when the
//! runner that owes the required status died before posting one: the merge box
//! refuses the merge for a context the tab does not even list. So the row set
//! is a union of three sources:
//!
//! 1. the base branch's required status contexts (branch protection), plus the
//!    `jankurai/proof` gate where it is enforced for the repository;
//! 2. the lanes the repository declares in its own `.jeryu/ci.toml` (schema 2),
//!    read from the base branch — a lane `x` publishes the context `<repo>/x`;
//! 3. every context actually reported on the head.
//!
//! A context from 1 or 2 with nothing reported on the head becomes an
//! `expected-not-reported` row that says how long the head has been waiting
//! and, when a gate runner's heartbeat names this head, that a claim or an
//! attempt exists for it.

use chrono::{DateTime, Utc};
use serde::Serialize;

use std::collections::{BTreeMap, BTreeSet};

use jeryu_core::PullRequest;

use crate::ci_bridge::audit_queue::{CI_DECLARATION_PATH, MAX_CI_DECLARATION_BYTES};
use crate::web::WebState;
use crate::web::auth::ANONYMOUS_LOGIN;
use crate::web::control_plane::{holds_gate_slot, is_online};

/// Where a row on the checks tab comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckSource {
    /// A required status context of the base branch's protection rule.
    Protection,
    /// A lane the repository declares in `.jeryu/ci.toml`, or a jeryu-side
    /// gate that applies to it.
    CiConfig,
    /// Nothing expects it; it was reported on the head.
    Reported,
}

impl CheckSource {
    pub(super) fn wire_name(self) -> &'static str {
        match self {
            Self::Protection => "required_by_protection",
            Self::CiConfig => "declared_by_ci_config",
            Self::Reported => "reported_only",
        }
    }
}

/// The contexts this head is expected to get, each with the source that
/// expects it. Protection wins over a declaration: a lane the base also
/// requires is reported as required.
pub(super) fn expected_contexts(
    state: &WebState,
    pr: &PullRequest,
    required: &BTreeSet<String>,
) -> BTreeMap<String, CheckSource> {
    let mut contexts = BTreeMap::new();
    for name in declared_contexts(state, pr) {
        contexts.insert(name, CheckSource::CiConfig);
    }
    for name in required {
        contexts.insert(name.clone(), CheckSource::Protection);
    }
    contexts
}

/// The contexts the repository's own CI declaration publishes, read from the
/// base branch: a pull request cannot declare its own lanes into existence,
/// the same rule the audit switch follows. Plus the jeryu-side gates that
/// apply here even when the protection rule does not name them.
fn declared_contexts(state: &WebState, pr: &PullRequest) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Some(text) = ci_declaration(state, pr) {
        for lane in lane_names(&text) {
            names.insert(format!("{}/{lane}", pr.repo));
        }
    }
    if super::posture::audit_gate_enforced_for(&pr.owner, &pr.repo) {
        names.insert(super::posture::JANKURAI_PROOF.to_string());
    }
    names
}

/// `<base>:.jeryu/ci.toml` out of the bare repository, when it is there and
/// small enough to read on a read path.
///
/// Read at the commit the base branch points at now, so a lane added to or
/// removed from the base after the pull request opened is the one shown. That
/// commit is immutable, so the answer is cached per commit: the checks route
/// is anonymously readable and the attention inbox asks it for every open
/// pull request, and a base that has not moved is not read by git again. A
/// base branch that is gone falls back to the commit the pull request recorded.
fn ci_declaration(state: &WebState, pr: &PullRequest) -> Option<String> {
    let opened = state.repo_manager.open_parts(&pr.owner, &pr.repo).ok()?;
    let dir = opened.path.as_path();
    let base = base_commit(state, dir, pr)?;
    let key = (opened.path.clone(), base.clone());
    if let Some(known) = cached(&DECLARATIONS, &key) {
        return known;
    }
    // Only a clean answer is remembered: the file is absent, or it was read.
    // A git that failed says nothing about the commit, so it is asked again.
    let listed = crate::web::shift::queue::git(
        &state.repo_manager.config().git_bin,
        dir,
        &[
            "ls-tree",
            "--end-of-options",
            &base,
            "--",
            CI_DECLARATION_PATH,
        ],
        &[],
        None,
    )
    .ok()?;
    let read = if listed.is_empty() {
        None
    } else {
        let object = format!("{base}:{CI_DECLARATION_PATH}");
        Some(read_ci_declaration(state, dir, &object)?)
    };
    remember(&DECLARATIONS, key, read.clone());
    read
}

/// The commit a pull request's base is read at: the base branch's tip, else
/// the recorded base commit when that is a commit id.
fn base_commit(state: &WebState, dir: &std::path::Path, pr: &PullRequest) -> Option<String> {
    let tip = format!("refs/heads/{}^{{commit}}", pr.base.ref_name);
    let resolved = crate::web::shift::queue::git(
        &state.repo_manager.config().git_bin,
        dir,
        &["rev-parse", "--verify", "--quiet", "--end-of-options", &tip],
        &[],
        None,
    )
    .ok()
    .map(|out| String::from_utf8_lossy(&out).trim().to_string())
    .filter(|id| is_commit_id(id));
    resolved.or_else(|| is_commit_id(&pr.base.sha).then(|| pr.base.sha.clone()))
}

fn read_ci_declaration(state: &WebState, dir: &std::path::Path, object: &str) -> Option<String> {
    let git_bin = &state.repo_manager.config().git_bin;
    let size = crate::web::shift::queue::git(
        git_bin,
        dir,
        &["cat-file", "-s", "--end-of-options", object],
        &[],
        None,
    )
    .ok()
    .and_then(|out| String::from_utf8_lossy(&out).trim().parse::<u64>().ok())?;
    if size > MAX_CI_DECLARATION_BYTES {
        return None;
    }
    crate::web::shift::queue::git(
        git_bin,
        dir,
        &["cat-file", "blob", "--end-of-options", object],
        &[],
        None,
    )
    .ok()
    .map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// The lane names a `.jeryu/ci.toml` declares (`[[lane]] name = "required"`).
/// A file in any other shape declares nothing rather than failing the read:
/// the tab still lists what protection requires and what was reported.
pub(in crate::web) fn lane_names(text: &str) -> Vec<String> {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(lanes) = table.get("lane").and_then(toml::Value::as_array) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for lane in lanes {
        let Some(name) = lane.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        let name = name.trim();
        // A lane name becomes part of a status context, so only the shape a
        // context is written in is taken from the file.
        let shaped = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if shaped && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// Since when this head has had nothing reported for a context: the head
/// commit's own time, or the pull request's last update when the commit cannot
/// be read.
pub(super) fn head_waiting_since(state: &WebState, pr: &PullRequest) -> DateTime<Utc> {
    head_commit_time(state, pr).unwrap_or(pr.updated_at)
}

fn head_commit_time(state: &WebState, pr: &PullRequest) -> Option<DateTime<Utc>> {
    if !is_commit_id(&pr.head.sha) {
        return None;
    }
    let opened = state.repo_manager.open_parts(&pr.owner, &pr.repo).ok()?;
    // A commit's time never changes: read it once per head.
    let key = (opened.path.clone(), pr.head.sha.clone());
    if let Some(known) = cached(&HEAD_TIMES, &key) {
        return known;
    }
    let read = read_head_commit_time(state, opened.path.as_path(), &pr.head.sha);
    remember(&HEAD_TIMES, key, read);
    read
}

fn read_head_commit_time(
    state: &WebState,
    dir: &std::path::Path,
    sha: &str,
) -> Option<DateTime<Utc>> {
    let git_bin = &state.repo_manager.config().git_bin;
    let out = crate::web::shift::queue::git(
        git_bin,
        dir,
        &["show", "-s", "--format=%cI", "--end-of-options", sha],
        &[],
        None,
    )
    .ok()?;
    DateTime::parse_from_rfc3339(String::from_utf8_lossy(&out).trim())
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn is_commit_id(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Facts read out of an immutable commit, keyed by the bare repository and the
/// commit id. Bounded: when full it starts over rather than growing.
type CommitFacts<V> = std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<(std::path::PathBuf, String), V>>,
>;
const MAX_CACHED_COMMITS: usize = 4096;

static DECLARATIONS: CommitFacts<Option<String>> = std::sync::LazyLock::new(Default::default);
static HEAD_TIMES: CommitFacts<Option<DateTime<Utc>>> = std::sync::LazyLock::new(Default::default);

fn cached<V: Clone>(cache: &CommitFacts<V>, key: &(std::path::PathBuf, String)) -> Option<V> {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
        .cloned()
}

fn remember<V>(cache: &CommitFacts<V>, key: (std::path::PathBuf, String), value: V) {
    let mut map = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if map.len() >= MAX_CACHED_COMMITS {
        map.clear();
    }
    map.insert(key, value);
}

/// What the forge knows about a gate working on this head, for a row no
/// status has been reported on.
#[derive(Debug, Clone, Serialize)]
pub(super) struct GateClaim {
    /// `running` while a runner's heartbeat claims this head, `attempted`
    /// when one reports a finished pass on it and nothing was posted.
    pub(super) state: &'static str,
    pub(super) recipe: String,
    /// When the claimed pass started, or when the finished one ended.
    pub(super) at: String,
    /// How the finished pass concluded; absent for a claim still running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) conclusion: Option<String>,
    /// Whether the claiming runner is still beating.
    pub(super) runner_online: bool,
    /// Which runner holds it. Absent for an anonymous reader: a slot's host
    /// is site topology, and this route is open to readers of public
    /// repositories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) runner_id: Option<String>,
    /// The runner's row on `/runners`; absent with [`Self::runner_id`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) runner_url: Option<String>,
    /// The login the runner posts its statuses as; absent as above.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reporter: Option<String>,
}

/// The gate claim or attempt a runner reports for this exact head, if any.
/// `viewer` is the reading account's login: a signed-in reader is told which
/// runner owes the status, an anonymous one only that one does.
pub(super) fn gate_claim(
    state: &WebState,
    pr: &PullRequest,
    viewer: Option<&str>,
) -> Option<GateClaim> {
    let repo = format!("{}/{}", pr.owner, pr.repo);
    let now = Utc::now();
    let records = state.gate_runners.snapshot();
    let named = viewer.is_some_and(|login| !login.is_empty() && login != ANONYMOUS_LOGIN);
    let mut claim: Option<GateClaim> = None;
    for record in &records {
        let beat = &record.heartbeat;
        if !holds_gate_slot(beat) {
            continue;
        }
        let online = is_online(record, now);
        let identity = |claim: &mut GateClaim| {
            if named {
                claim.runner_id = Some(beat.runner_id.clone());
                claim.runner_url = Some(format!(
                    "/runners?runners={}",
                    query_escape(&beat.runner_id)
                ));
                claim.reporter = Some(record.reporter.clone());
            }
        };
        if let Some(task) = beat
            .current
            .as_ref()
            .filter(|task| task.repo == repo && task.sha == pr.head.sha)
        {
            let mut running = GateClaim {
                state: "running",
                recipe: task.recipe.clone(),
                at: task.started_at.to_rfc3339(),
                conclusion: None,
                runner_online: online,
                runner_id: None,
                runner_url: None,
                reporter: None,
            };
            identity(&mut running);
            // A live claim answers the question; a finished pass only does
            // while nothing claims the head.
            return Some(running);
        }
        if let Some(last) = beat
            .last
            .as_ref()
            .filter(|last| last.repo == repo && last.sha == pr.head.sha)
        {
            let newer = claim
                .as_ref()
                .is_none_or(|known| known.at <= last.finished_at.to_rfc3339());
            if newer {
                let mut attempted = GateClaim {
                    state: "attempted",
                    recipe: last.recipe.clone(),
                    at: last.finished_at.to_rfc3339(),
                    conclusion: Some(last.conclusion.clone()),
                    runner_online: online,
                    runner_id: None,
                    runner_url: None,
                    reporter: None,
                };
                identity(&mut attempted);
                claim = Some(attempted);
            }
        }
    }
    claim
}

/// Percent-escape a runner id into a query value. A runner id is written
/// `node-2/slot0`, and the slash has to survive as data.
fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
