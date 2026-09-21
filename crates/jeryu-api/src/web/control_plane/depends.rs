//! `depends_on`: repo-to-repo edges read from Cargo manifests.
//!
//! The repo graph used to draw containment only (a repo, its pull requests,
//! their checks), so nothing said which repository a repository is built on.
//! Ground truth for that is each repository's Cargo manifests at its indexed
//! default-branch ref: every git dependency whose URL names a repository this
//! forge hosts is one `depends_on` edge, carrying the tag it pins and whether
//! that tag is the newest the dependency has published.
//!
//! Reading 90 repositories costs a git process per manifest, so the answer is
//! cached for a minute; the graph itself is rebuilt per request from the read
//! model, which is cheap. Edges are emitted as they are found — a cycle
//! between two repositories is real information, and the renderer decides how
//! to draw it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jeryu_core::Repository;

use crate::web::WebState;
use crate::web::pipeline::pins::{manifest_pins, resolve_pin};
use crate::web::shift::run_git as git;

/// The manifest reads are bounded by this, not by the request rate.
const CACHE_FOR: Duration = Duration::from_secs(60);
/// A repository with more manifests than this is read partially rather than
/// holding the whole graph hostage.
const MAX_MANIFESTS: usize = 64;

/// One git dependency of one repository, resolved against the forge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DependsEdge {
    /// `owner/name` of the repository whose manifest holds the dependency.
    pub consumer: String,
    /// `owner/name` of the hosted repository it depends on.
    pub dependency: String,
    /// The manifest the dependency is written in.
    pub source: String,
    /// `tag` or `commit` (a `rev` pin).
    pub kind: &'static str,
    /// The tag or sha as written.
    pub pinned_ref: String,
    /// The newest tag the dependency publishes in the pinned tag's series.
    pub newest_ref: Option<String>,
    /// Whether [`Self::pinned_ref`] is that newest tag. `None` when the pin is
    /// a `rev`, or the dependency publishes no tag of that shape.
    pub pin_is_newest: Option<bool>,
    /// The pin compared against the dependency's default branch, by the same
    /// resolver `GET /api/v1/pins` uses: `current`, `behind`,
    /// `behind_not_green`, `diverged` or `unknown`.
    pub pin_state: &'static str,
    /// Commits on the dependency's default branch the pin does not reach;
    /// `None` when the pin could not be compared.
    pub behind: Option<u64>,
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

/// The root manifest and one per crate: `Cargo.toml`, `crates/<name>/Cargo.toml`.
fn is_manifest(path: &str) -> bool {
    path == "Cargo.toml"
        || path
            .strip_prefix("crates/")
            .and_then(|rest| rest.strip_suffix("/Cargo.toml"))
            .is_some_and(|name| !name.contains('/'))
}

fn manifest_paths(git_bin: &str, dir: &Path, branch: &str) -> Vec<String> {
    let tree = format!("refs/heads/{branch}");
    git(
        git_bin,
        dir,
        &["ls-tree", "--name-only", "-r", &tree],
        &[],
        None,
    )
    .map(|out| {
        text_of(out)
            .lines()
            .filter(|path| is_manifest(path))
            .take(MAX_MANIFESTS)
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// A tag's series: everything up to its first digit, so
/// `jeryu-core-v5.0.0-split.7` and `jeryu-core-v4.2.0` share `jeryu-core-v`.
/// Comparing inside a series keeps an unrelated tag line (a date stamp, a web
/// bundle tag) from declaring every crate pin stale.
pub(crate) fn tag_series(tag: &str) -> &str {
    let end = tag
        .char_indices()
        .find(|(_, ch)| ch.is_ascii_digit())
        .map_or(tag.len(), |(at, _)| at);
    &tag[..end]
}

/// The newest tag the repository publishes in `series`: by the date the tag
/// points at, which is how a release line actually advances, and by version
/// order when two tags share a date (a tag cut minutes after its predecessor
/// carries the same commit date at second resolution).
pub(crate) fn newest_tag(git_bin: &str, dir: &Path, series: &str) -> Option<String> {
    let pattern = format!("refs/tags/{series}*");
    let listed = git(
        git_bin,
        dir,
        &[
            "for-each-ref",
            "--sort=-v:refname",
            "--sort=-creatordate",
            "--format=%(refname:short)",
            &pattern,
        ],
        &[],
        None,
    )
    .ok()?;
    text_of(listed)
        .lines()
        .find(|tag| tag_series(tag) == series)
        .map(str::to_string)
}

/// The hosted repository a dependency names: under the consumer's owner first,
/// else the only hosted repository with that name.
fn hosted<'a>(repos: &'a [Repository], owner: &str, name: &str) -> Option<&'a Repository> {
    let mut named = repos.iter().filter(|repo| repo.name == name);
    let same_owner = named.clone().find(|repo| repo.owner == owner);
    same_owner.or_else(|| match (named.next(), named.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    })
}

fn repo_edges(state: &WebState, repos: &[Repository], consumer: &Repository) -> Vec<DependsEdge> {
    let git_bin = &state.repo_manager.config().git_bin;
    let Ok(opened) = state
        .repo_manager
        .open_parts(&consumer.owner, &consumer.name)
    else {
        return Vec::new();
    };
    let dir = opened.path.as_path();
    let branch = &consumer.default_branch;
    let mut edges: Vec<DependsEdge> = Vec::new();
    for manifest in manifest_paths(git_bin, dir, branch) {
        let text = show_file(git_bin, dir, branch, &manifest).unwrap_or_default();
        for pin in manifest_pins(&text) {
            if pin.name == consumer.name {
                continue;
            }
            let Some(dependency) = hosted(repos, &consumer.owner, &pin.name) else {
                continue;
            };
            if edges
                .iter()
                .any(|edge| edge.dependency == dependency.full_name)
            {
                continue;
            }
            let (newest_ref, pin_is_newest) = match pin.kind {
                "tag" => newest_of(state, dependency, &pin.pinned_ref),
                _ => (None, None),
            };
            let resolved = resolve_pin(state, consumer, dependency, &manifest, &pin);
            edges.push(DependsEdge {
                consumer: consumer.full_name.clone(),
                dependency: dependency.full_name.clone(),
                source: manifest.clone(),
                kind: pin.kind,
                pinned_ref: pin.pinned_ref.clone(),
                newest_ref,
                pin_is_newest,
                pin_state: resolved.state,
                behind: (resolved.state != "unknown").then_some(resolved.behind),
            });
        }
    }
    edges
}

fn newest_of(
    state: &WebState,
    dependency: &Repository,
    pinned_ref: &str,
) -> (Option<String>, Option<bool>) {
    let git_bin = &state.repo_manager.config().git_bin;
    let Ok(opened) = state
        .repo_manager
        .open_parts(&dependency.owner, &dependency.name)
    else {
        return (None, None);
    };
    let newest = newest_tag(git_bin, opened.path.as_path(), tag_series(pinned_ref));
    let is_newest = newest.as_deref().map(|newest| newest == pinned_ref);
    (newest, is_newest)
}

/// Every hosted repository's git dependencies, read from the bare repositories
/// right now. Runs git: call it off the async workers.
pub(crate) fn collect_depends(state: &WebState) -> Vec<DependsEdge> {
    let repos = state.core.list_repositories(None);
    repos
        .iter()
        .filter(|repo| !repo.archived && !repo.disabled)
        .flat_map(|consumer| repo_edges(state, &repos, consumer))
        .collect()
}

/// The last answer, reused for [`CACHE_FOR`]: the graph is rebuilt per
/// request, but walking every repository's manifests is not.
type CachedEdges = Option<(Instant, Vec<DependsEdge>)>;

#[derive(Clone, Default)]
pub(crate) struct DependsCache {
    inner: Arc<Mutex<CachedEdges>>,
}

/// The cached edges, or a fresh read.
pub(crate) fn depends_snapshot(state: &WebState) -> Vec<DependsEdge> {
    let lock = || {
        state
            .repo_depends
            .inner
            .lock()
            .expect("repo depends cache mutex poisoned")
    };
    let cached = lock()
        .as_ref()
        .filter(|(at, _)| at.elapsed() < CACHE_FOR)
        .map(|(_, edges)| edges.clone());
    if let Some(edges) = cached {
        return edges;
    }
    let edges = collect_depends(state);
    *lock() = Some((Instant::now(), edges.clone()));
    edges
}

/// The edge metadata a renderer needs to show a pin and its staleness.
pub(crate) fn edge_metadata(edge: &DependsEdge) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::from([
        ("pinKind".to_string(), edge.kind.to_string()),
        ("pinnedRef".to_string(), edge.pinned_ref.clone()),
        ("pinState".to_string(), edge.pin_state.to_string()),
        ("source".to_string(), edge.source.clone()),
    ]);
    if let Some(newest) = &edge.newest_ref {
        metadata.insert("newestRef".to_string(), newest.clone());
    }
    if let Some(is_newest) = edge.pin_is_newest {
        metadata.insert("pinIsNewest".to_string(), is_newest.to_string());
    }
    if let Some(behind) = edge.behind {
        metadata.insert("behind".to_string(), behind.to_string());
    }
    metadata
}
