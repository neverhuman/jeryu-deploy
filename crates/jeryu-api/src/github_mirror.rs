//! Direct merge-to-GitHub main mirroring.
//!
//! When a PR merges into a repo's default branch on the local forge, the merge
//! handler pushes the LIVE branch tip straight to the configured GitHub
//! repository (`github.com/<github_slug>` main). Targets come from the split
//! manifests: a `[[repo]]` entry participates iff it carries `github_slug`,
//! `jeryu_slug`, and `mirror_github_main = true`.
//!
//! Auth rides the host's proven shim: the destination URL is built as
//! `https://x-access-token:jeryussh@github.com/<slug>.git`, which the global
//! gitconfig `url."git@github-mirror:".insteadOf` rewrite turns into an SSH
//! push with the neverhuman deploy key. Nothing secret-looking is stored in
//! the manifest (the relay token is a fixed public dummy).
//!
//! Tags mirror the same way: a tag push sends tags GitHub does not have yet.
//! An existing GitHub tag is NEVER moved or deleted, so a tag that differs is
//! reported, not forced.
//!
//! [`GithubMirror::reconcile`] is the catch-up: it compares the forge's branch
//! and tags with GitHub's, fast-forwards GitHub when it is behind, and when
//! GitHub is AHEAD or diverged it pushes nothing and names the commits only
//! GitHub has, so a person can decide what happens to them.
//!
//! Failure isolation: a GitHub push failure NEVER fails the merge. The outcome
//! is recorded as a `jeryu/github-mirror` check-run on the merged tip so the
//! posture is visible next to CI, matching the legacy external relay's
//! convention.

#![cfg(feature = "web")]

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Hard wall-clock bound for one push; a hung network push must never wedge
/// the merge handler (the push runs synchronously inside it).
const PUSH_TIMEOUT: Duration = Duration::from_secs(60);

/// Check-run name used to record push outcomes (the relay's own convention).
pub const MIRROR_CHECK_NAME: &str = "jeryu/github-mirror";

/// Check-run name used to alarm when GitHub holds commits the forge does not.
pub const MIRROR_DIVERGED_CHECK_NAME: &str = "jeryu/github-mirror-divergence";

/// How many GitHub-only commits a divergence report names before it truncates;
/// a reader needs the shape of the drift, not a whole branch.
const MAX_NAMED_COMMITS: usize = 20;

/// Where a reconcile parks the GitHub branch it fetched for comparison. Outside
/// `refs/heads/` and `refs/tags/`, so nothing the forge serves or mirrors moves
/// because the comparison ran.
const COMPARE_REF: &str = "refs/github-mirror/compare";

#[derive(Clone, Debug, Default)]
pub struct GithubMirror {
    enabled: bool,
    /// Keyed by lowercased local slug `owner/name` (the manifest `jeryu_slug`).
    targets: BTreeMap<String, GithubMirrorTarget>,
}

#[derive(Clone, Debug)]
pub struct GithubMirrorTarget {
    pub github_slug: String,
    pub branch: String,
    /// Test seam: a local path or URL that replaces the GitHub destination.
    pub destination_override: Option<String>,
    /// Tag patterns the mirror never pushes and never reports as drift
    /// (`mirror_tags_exclude`; `*` matches any run of characters). A repository
    /// whose GitHub copy publishes releases on a pushed `v*` tag lists `v*`, so a
    /// release tag reaches GitHub only when a person pushes it there.
    pub tag_exclude: Vec<String>,
}

/// True when `tag` matches one of `patterns` (`*` matches any run of characters).
fn tag_excluded(patterns: &[String], tag: &str) -> bool {
    patterns.iter().any(|pattern| glob_match(pattern, tag))
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let tail: Vec<&str> = parts.collect();
    let Some((last, middle)) = tail.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirrorPushOutcome {
    Pushed { tip: String },
    Skipped(String),
    Failed(String),
}

/// How the GitHub branch stands against the forge branch, after whatever the
/// reconcile was allowed to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MirrorSync {
    /// GitHub main is the forge main.
    InSync,
    /// GitHub is missing forge commits and a push could not be made.
    Behind,
    /// GitHub holds every forge commit plus commits of its own.
    Ahead,
    /// Both sides hold commits the other does not.
    Diverged,
    /// GitHub could not be read (network, auth, no key on this host).
    Unknown,
}

/// A tag the forge and GitHub disagree about. Never resolved by the mirror:
/// moving or deleting a GitHub tag is a decision only a person makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagDrift {
    pub tag: String,
    pub forge_oid: Option<String>,
    pub github_oid: Option<String>,
    /// One sentence naming what differs, for the alarm and the repo page.
    pub detail: String,
}

/// What one tag mirroring pass did and found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MirrorTagOutcome {
    /// Tags newly created on GitHub by this pass.
    pub pushed: Vec<String>,
    /// Tags GitHub already holds with the forge's oid.
    pub already_present: Vec<String>,
    /// Tags that differ, are missing on the forge, or failed to push.
    pub drift: Vec<TagDrift>,
    /// Set when GitHub could not be read at all; then nothing was attempted.
    pub error: Option<String>,
}

/// The full picture of one enrolled repository against its GitHub mirror.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorReconcile {
    pub github_slug: String,
    pub branch: String,
    pub forge_head: Option<String>,
    pub github_head: Option<String>,
    pub state: MirrorSync,
    /// Whether this pass fast-forwarded GitHub.
    pub caught_up: bool,
    /// Commits GitHub has that the forge does not, newest first, capped.
    pub github_only_commits: Vec<String>,
    pub tags: MirrorTagOutcome,
    /// Why the state is `Unknown`, or why a fast-forward did not land.
    pub error: Option<String>,
}

impl MirrorReconcile {
    /// True when something is wrong that only a person can settle: GitHub
    /// ahead or diverged, or a tag the mirror refuses to move.
    pub fn needs_a_person(&self) -> bool {
        matches!(self.state, MirrorSync::Ahead | MirrorSync::Diverged)
            || !self.tags.drift.is_empty()
    }
}

#[derive(Debug, serde::Deserialize)]
struct MirrorManifest {
    repo: Option<Vec<MirrorManifestRepo>>,
}

#[derive(Debug, serde::Deserialize)]
struct MirrorManifestRepo {
    github_slug: Option<String>,
    jeryu_slug: Option<String>,
    default_branch: Option<String>,
    #[serde(default)]
    mirror_github_main: bool,
    #[serde(default)]
    mirror_tags_exclude: Vec<String>,
}

impl GithubMirror {
    /// Load targets from split manifests. Absent/unparsable manifests, or
    /// `JERYU_GITHUB_PUSH=0`, yields a disabled mirror (every push skips).
    pub fn load(manifests: &[PathBuf]) -> Self {
        if std::env::var("JERYU_GITHUB_PUSH").as_deref() == Ok("0") {
            return Self::default();
        }
        Self::from_manifests(manifests)
    }

    fn from_manifests(manifests: &[PathBuf]) -> Self {
        if manifests.is_empty() {
            return Self::default();
        }
        let mut targets = BTreeMap::new();
        for path in manifests {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(parsed) = toml::from_str::<MirrorManifest>(&text) else {
                continue;
            };
            for repo in parsed.repo.unwrap_or_default() {
                let (Some(github_slug), Some(jeryu_slug)) = (repo.github_slug, repo.jeryu_slug)
                else {
                    continue;
                };
                if !repo.mirror_github_main {
                    continue;
                }
                targets.insert(
                    jeryu_slug.to_ascii_lowercase(),
                    GithubMirrorTarget {
                        github_slug,
                        branch: repo.default_branch.unwrap_or_else(|| "main".to_string()),
                        destination_override: None,
                        tag_exclude: repo.mirror_tags_exclude,
                    },
                );
            }
        }
        Self {
            enabled: !targets.is_empty(),
            targets,
        }
    }

    /// Embedding/test seam: build a mirror from explicit targets (e.g. a local
    /// bare destination via `destination_override`).
    pub fn with_targets(targets: BTreeMap<String, GithubMirrorTarget>) -> Self {
        Self {
            enabled: !targets.is_empty(),
            targets,
        }
    }

    /// Whether this mirror has any target at all: false under the
    /// `JERYU_GITHUB_PUSH=0` kill switch and with no manifest.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn target(&self, owner: &str, name: &str) -> Option<&GithubMirrorTarget> {
        if !self.enabled {
            return None;
        }
        self.targets
            .get(&format!("{}/{}", owner, name).to_ascii_lowercase())
    }

    /// Push the LIVE tip of the target branch in `bare` to the GitHub remote.
    ///
    /// Resolves the branch tip at call time (NOT the merge oid) because the
    /// post-push bridge may have appended a `chore(release)` autoversion commit
    /// after the merge moved the ref. Fast-forward only: no `--force`.
    pub fn push_branch(
        &self,
        git_bin: &str,
        bare: &Path,
        owner: &str,
        name: &str,
    ) -> MirrorPushOutcome {
        let Some(target) = self.target(owner, name) else {
            return MirrorPushOutcome::Skipped(format!(
                "{owner}/{name} is not a github-mirror target"
            ));
        };
        let tip = match run_bounded(
            git_bin,
            &["rev-parse", &format!("refs/heads/{}", target.branch)],
            bare,
        ) {
            Ok(out) => out.trim().to_string(),
            Err(err) => return MirrorPushOutcome::Failed(format!("resolve tip: {err}")),
        };
        if tip.is_empty() {
            return MirrorPushOutcome::Failed(format!(
                "refs/heads/{} did not resolve in {}",
                target.branch,
                bare.display()
            ));
        }
        let dest = target.destination_override.clone().unwrap_or_else(|| {
            format!(
                "https://x-access-token:jeryussh@github.com/{}.git",
                target.github_slug
            )
        });
        let refspec = format!("{}:refs/heads/{}", tip, target.branch);
        match run_bounded(git_bin, &["push", &dest, &refspec], bare) {
            Ok(_) => MirrorPushOutcome::Pushed { tip },
            Err(err) => MirrorPushOutcome::Failed(redact(&err)),
        }
    }

    /// Mirror tags to GitHub, creating only what GitHub does not have yet.
    ///
    /// `only` limits the pass to the tags a push just moved; `None` considers
    /// every tag in the bare repository (what the reconcile wants). A tag
    /// GitHub already holds at a different oid, or holds while the forge does
    /// not, is recorded as drift: the mirror never moves or deletes a GitHub
    /// tag, because the forge cannot know what was cut from it.
    pub fn push_tags(
        &self,
        git_bin: &str,
        bare: &Path,
        owner: &str,
        name: &str,
        only: Option<&[String]>,
    ) -> MirrorTagOutcome {
        let Some(target) = self.target(owner, name) else {
            return MirrorTagOutcome::default();
        };
        let dest = destination(target);
        let remote = match remote_refs(git_bin, bare, &dest) {
            Ok(refs) => refs,
            Err(err) => {
                return MirrorTagOutcome {
                    error: Some(redact(&err)),
                    ..MirrorTagOutcome::default()
                };
            }
        };
        let forge = match local_tags(git_bin, bare) {
            Ok(tags) => tags,
            Err(err) => {
                return MirrorTagOutcome {
                    error: Some(redact(&err)),
                    ..MirrorTagOutcome::default()
                };
            }
        };
        tag_pass(
            git_bin,
            bare,
            &dest,
            &forge,
            &remote,
            only,
            &target.tag_exclude,
        )
    }

    /// Compare the forge with GitHub and fast-forward GitHub when it is behind.
    ///
    /// Returns `None` for a repository that is not enrolled (including when the
    /// mirror is off, so `JERYU_GITHUB_PUSH=0` reconciles nothing). Never
    /// forces: when GitHub is ahead or diverged it pushes nothing and names the
    /// commits only GitHub has, which is what the alarm quotes.
    pub fn reconcile(
        &self,
        git_bin: &str,
        bare: &Path,
        owner: &str,
        name: &str,
    ) -> Option<MirrorReconcile> {
        let target = self.target(owner, name)?;
        let dest = destination(target);
        let branch_ref = format!("refs/heads/{}", target.branch);
        let mut report = MirrorReconcile {
            github_slug: target.github_slug.clone(),
            branch: target.branch.clone(),
            forge_head: None,
            github_head: None,
            state: MirrorSync::Unknown,
            caught_up: false,
            github_only_commits: Vec::new(),
            tags: MirrorTagOutcome::default(),
            error: None,
        };
        let forge_head = run_bounded(git_bin, &["rev-parse", &branch_ref], bare)
            .map(|out| out.trim().to_string())
            .ok()
            .filter(|oid| !oid.is_empty());
        report.forge_head = forge_head.clone();
        let remote = match remote_refs(git_bin, bare, &dest) {
            Ok(refs) => refs,
            Err(err) => {
                report.error = Some(redact(&err));
                report.tags.error = report.error.clone();
                return Some(report);
            }
        };
        report.github_head = remote.get(&branch_ref).cloned();
        report.tags = match local_tags(git_bin, bare) {
            Ok(forge_tags) => tag_pass(
                git_bin,
                bare,
                &dest,
                &forge_tags,
                &remote,
                None,
                &target.tag_exclude,
            ),
            Err(err) => MirrorTagOutcome {
                error: Some(redact(&err)),
                ..MirrorTagOutcome::default()
            },
        };
        let Some(forge_head) = forge_head else {
            report.error = Some(format!(
                "{branch_ref} did not resolve in {}",
                bare.display()
            ));
            return Some(report);
        };
        let Some(github_head) = report.github_head.clone() else {
            // GitHub has no such branch yet: creating it is a fast-forward.
            report.state = match push_ref(git_bin, bare, &dest, &forge_head, &branch_ref) {
                Ok(()) => {
                    report.caught_up = true;
                    MirrorSync::InSync
                }
                Err(err) => {
                    report.error = Some(err);
                    MirrorSync::Behind
                }
            };
            return Some(report);
        };
        if github_head == forge_head {
            report.state = MirrorSync::InSync;
            return Some(report);
        }
        // The GitHub commit is usually absent locally; fetch it under a ref of
        // our own so ancestry can be decided without guessing.
        if let Err(err) = run_bounded(
            git_bin,
            &[
                "fetch",
                "--no-tags",
                &dest,
                &format!("+{branch_ref}:{COMPARE_REF}"),
            ],
            bare,
        ) {
            report.error = Some(redact(&err));
            return Some(report);
        }
        if is_ancestor(git_bin, bare, &github_head, &forge_head) {
            report.state = match push_ref(git_bin, bare, &dest, &forge_head, &branch_ref) {
                Ok(()) => {
                    report.caught_up = true;
                    MirrorSync::InSync
                }
                Err(err) => {
                    report.error = Some(err);
                    MirrorSync::Behind
                }
            };
            return Some(report);
        }
        report.github_only_commits = commits_only_on(git_bin, bare, &forge_head, &github_head);
        report.state = if is_ancestor(git_bin, bare, &forge_head, &github_head) {
            MirrorSync::Ahead
        } else {
            MirrorSync::Diverged
        };
        Some(report)
    }
}

/// The GitHub URL a target pushes to, or the test seam standing in for it.
fn destination(target: &GithubMirrorTarget) -> String {
    target.destination_override.clone().unwrap_or_else(|| {
        format!(
            "https://x-access-token:jeryussh@github.com/{}.git",
            target.github_slug
        )
    })
}

/// One tag comparison pass, shared by the tag-push path and the reconcile.
fn tag_pass(
    git_bin: &str,
    bare: &Path,
    dest: &str,
    forge: &BTreeMap<String, String>,
    remote: &BTreeMap<String, String>,
    only: Option<&[String]>,
    exclude: &[String],
) -> MirrorTagOutcome {
    let wanted = |tag: &str| {
        only.is_none_or(|names| names.iter().any(|name| name == tag)) && !tag_excluded(exclude, tag)
    };
    let mut outcome = MirrorTagOutcome::default();
    for (tag, forge_oid) in forge {
        if !wanted(tag) {
            continue;
        }
        let tag_ref = format!("refs/tags/{tag}");
        match remote.get(&tag_ref) {
            Some(github_oid) if github_oid == forge_oid => {
                outcome.already_present.push(tag.clone())
            }
            Some(github_oid) => outcome.drift.push(TagDrift {
                tag: tag.clone(),
                forge_oid: Some(forge_oid.clone()),
                github_oid: Some(github_oid.clone()),
                detail: format!(
                    "GitHub holds {tag} at {} and the forge at {forge_oid}; the mirror does not \
                     move a tag GitHub already published",
                    github_oid
                ),
            }),
            None => match push_ref(git_bin, bare, dest, forge_oid, &tag_ref) {
                Ok(()) => outcome.pushed.push(tag.clone()),
                Err(err) => outcome.drift.push(TagDrift {
                    tag: tag.clone(),
                    forge_oid: Some(forge_oid.clone()),
                    github_oid: None,
                    detail: err,
                }),
            },
        }
    }
    // Tags only GitHub has are reported too: the forge is the truth, and a tag
    // that exists only downstream is drift somebody has to explain.
    for (tag_ref, github_oid) in remote {
        let Some(tag) = tag_ref.strip_prefix("refs/tags/") else {
            continue;
        };
        if forge.contains_key(tag) || !wanted(tag) {
            continue;
        }
        outcome.drift.push(TagDrift {
            tag: tag.to_string(),
            forge_oid: None,
            github_oid: Some(github_oid.clone()),
            detail: format!("GitHub holds {tag} at {github_oid} and the forge has no such tag"),
        });
    }
    outcome
}

/// Push one oid to one destination ref, fast-forward only (never `--force`).
fn push_ref(
    git_bin: &str,
    bare: &Path,
    dest: &str,
    oid: &str,
    dest_ref: &str,
) -> Result<(), String> {
    let refspec = format!("{oid}:{dest_ref}");
    run_bounded(git_bin, &["push", dest, &refspec], bare)
        .map(|_| ())
        .map_err(|err| redact(&err))
}

/// Every ref GitHub advertises, as `refs/...` -> oid. Peeled `^{}` lines are
/// dropped: an annotated tag is compared by its tag object, which is what a
/// push creates.
fn remote_refs(git_bin: &str, bare: &Path, dest: &str) -> Result<BTreeMap<String, String>, String> {
    let out = run_bounded(git_bin, &["ls-remote", dest], bare)?;
    Ok(out
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, name)| !name.ends_with("^{}"))
        .map(|(oid, name)| (name.trim().to_string(), oid.trim().to_string()))
        .collect())
}

/// Every tag in the bare repository, as short name -> oid.
fn local_tags(git_bin: &str, bare: &Path) -> Result<BTreeMap<String, String>, String> {
    let out = run_bounded(
        git_bin,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname:short)",
            "refs/tags",
        ],
        bare,
    )?;
    Ok(out
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(oid, name)| (name.trim().to_string(), oid.trim().to_string()))
        .collect())
}

/// Whether `ancestor` is reachable from `descendant` in the bare repository.
fn is_ancestor(git_bin: &str, bare: &Path, ancestor: &str, descendant: &str) -> bool {
    run_bounded(
        git_bin,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        bare,
    )
    .is_ok()
}

/// The commits reachable from `theirs` but not from `ours`, newest first.
fn commits_only_on(git_bin: &str, bare: &Path, ours: &str, theirs: &str) -> Vec<String> {
    let range = format!("{ours}..{theirs}");
    run_bounded(
        git_bin,
        &[
            "rev-list",
            &format!("--max-count={MAX_NAMED_COMMITS}"),
            &range,
        ],
        bare,
    )
    .map(|out| out.lines().map(|line| line.trim().to_string()).collect())
    .unwrap_or_default()
}

/// Run git with prompts disabled and a hard timeout; returns stdout on success.
fn run_bounded(git_bin: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let mut child = std::process::Command::new(git_bin)
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=10",
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| format!("spawn {git_bin}: {err}"))?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_string(&mut stdout);
                }
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut stderr);
                }
                if status.success() {
                    return Ok(stdout);
                }
                return Err(format!("git {} failed: {}", args.join(" "), stderr.trim()));
            }
            Ok(None) => {
                if start.elapsed() > PUSH_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "git {} timed out after {}s",
                        args.join(" "),
                        PUSH_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(format!("wait: {err}")),
        }
    }
}

/// Strip the relay-token userinfo from any URL echoed in git errors.
///
/// Single forward pass: everything between `x-access-token:` and the next `@`
/// becomes `***`, and scanning resumes AFTER the rewritten span so the
/// replacement can never re-match itself.
fn redact(text: &str) -> String {
    const MARK: &str = "x-access-token:";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(MARK) {
        let after = start + MARK.len();
        out.push_str(&rest[..after]);
        let tail = &rest[after..];
        match tail.find('@') {
            Some(at) => {
                out.push_str("***");
                rest = &tail[at..];
            }
            None => {
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_switch_disables_targets() {
        // Pure predicate check (mirrors ci_bridge::mock_flag_set style): the
        // env-driven branch is exercised via load() in integration tests; here
        // we prove an empty target set always skips.
        let mirror = GithubMirror::default();
        assert!(mirror.target("jeryu", "jeryu-core").is_none());
    }

    #[test]
    fn manifest_targets_match_jeryu_slug_case_insensitively() {
        let mut targets = BTreeMap::new();
        targets.insert(
            "jeryu/jeryu-core".to_string(),
            GithubMirrorTarget {
                github_slug: "neverhuman/jeryu-core".to_string(),
                branch: "main".to_string(),
                destination_override: None,
                tag_exclude: Vec::new(),
            },
        );
        let mirror = GithubMirror::with_targets(targets);
        assert!(mirror.target("Jeryu", "JERYU-CORE").is_some());
        assert!(mirror.target("jeryu", "other").is_none());
    }

    #[test]
    fn manifest_targets_aggregate_from_multiple_manifests() {
        let root = tempfile::tempdir().expect("manifest dir");
        let jeryu_manifest = root.path().join("jeryu.toml");
        let jekko_manifest = root.path().join("jekko.toml");
        std::fs::write(
            &jeryu_manifest,
            r#"
[[repo]]
github_slug = "neverhuman/jeryu-core"
jeryu_slug = "jeryu/jeryu-core"
default_branch = "main"
mirror_github_main = true
"#,
        )
        .expect("write jeryu manifest");
        std::fs::write(
            &jekko_manifest,
            r#"
[[repo]]
github_slug = "neverhuman/jekko-core"
jeryu_slug = "jeryu/jekko-core"
default_branch = "main"
mirror_github_main = true

[[repo]]
github_slug = "neverhuman/jekko-scratch"
jeryu_slug = "jeryu/jekko-scratch"
default_branch = "main"
mirror_github_main = false
"#,
        )
        .expect("write jekko manifest");

        let mirror = GithubMirror::from_manifests(&[jeryu_manifest, jekko_manifest]);
        assert_eq!(
            mirror
                .target("jeryu", "jeryu-core")
                .map(|target| target.github_slug.as_str()),
            Some("neverhuman/jeryu-core")
        );
        assert_eq!(
            mirror
                .target("jeryu", "jekko-core")
                .map(|target| target.github_slug.as_str()),
            Some("neverhuman/jekko-core")
        );
        assert!(
            mirror.target("jeryu", "jekko-scratch").is_none(),
            "mirror_github_main=false entries are ignored"
        );
    }

    #[test]
    fn redact_strips_relay_token() {
        let raw = "fatal: unable to access 'https://x-access-token:jeryussh@github.com/x.git'";
        let cleaned = redact(raw);
        assert!(cleaned.contains("x-access-token:***@github.com"));
        assert!(!cleaned.contains("jeryussh"));
    }

    // --- tags and reconcile, against a local bare repo standing in for GitHub ---

    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=mirror test",
                "-c",
                "user.email=mirror@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// A forge bare repo with one commit on main and the tag `v1`, a work tree
    /// to add more commits from, and an empty bare repo standing in for GitHub.
    struct Fixture {
        _root: tempfile::TempDir,
        work: PathBuf,
        forge: PathBuf,
        github: PathBuf,
    }

    impl Fixture {
        fn seed(name: &str) -> Self {
            let root = tempfile::tempdir().expect("fixture root");
            let work = root.path().join(format!("{name}-work"));
            let forge = root.path().join(format!("{name}-forge.git"));
            let github = root.path().join(format!("{name}-github.git"));
            std::fs::create_dir_all(&work).expect("work dir");
            git(root.path(), &["init", "--bare", forge.to_str().unwrap()]);
            git(root.path(), &["init", "--bare", github.to_str().unwrap()]);
            git(&work, &["init", "--initial-branch=main"]);
            let fixture = Self {
                _root: root,
                work,
                forge,
                github,
            };
            fixture.commit("one");
            git(&fixture.work, &["tag", "v1"]);
            git(
                &fixture.work,
                &["push", fixture.forge.to_str().unwrap(), "main", "v1"],
            );
            fixture
        }

        fn commit(&self, text: &str) -> String {
            std::fs::write(self.work.join("file.txt"), text).expect("write file");
            git(&self.work, &["add", "."]);
            git(&self.work, &["commit", "-m", text]);
            git(&self.work, &["rev-parse", "HEAD"])
        }

        /// Push the work tree's current main (and any named refs) to a bare repo.
        fn push(&self, to: &Path, refs: &[&str]) {
            let mut args = vec!["push", to.to_str().unwrap()];
            args.extend_from_slice(refs);
            git(&self.work, &args);
        }

        fn mirror(&self) -> GithubMirror {
            self.mirror_excluding(&[])
        }

        fn mirror_excluding(&self, tag_exclude: &[&str]) -> GithubMirror {
            let mut targets = BTreeMap::new();
            targets.insert(
                "acme/demo".to_string(),
                GithubMirrorTarget {
                    github_slug: "neverhuman/demo".to_string(),
                    branch: "main".to_string(),
                    destination_override: Some(self.github.to_string_lossy().into_owned()),
                    tag_exclude: tag_exclude.iter().map(|p| p.to_string()).collect(),
                },
            );
            GithubMirror::with_targets(targets)
        }

        fn github_ref(&self, name: &str) -> Option<String> {
            let out = std::process::Command::new("git")
                .args(["rev-parse", name])
                .current_dir(&self.github)
                .output()
                .expect("github rev-parse");
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
    }

    #[test]
    fn a_pushed_tag_is_created_on_github_and_an_existing_one_is_never_moved() {
        if !git_available() {
            return;
        }
        let fixture = Fixture::seed("tag-push");
        let mirror = fixture.mirror();

        let created = mirror.push_tags("git", &fixture.forge, "acme", "demo", Some(&["v1".into()]));
        assert_eq!(created.pushed, vec!["v1".to_string()]);
        assert!(created.drift.is_empty(), "clean create: {created:?}");
        let published = fixture
            .github_ref("refs/tags/v1")
            .expect("GitHub holds the new tag");

        // The forge moves v1 to another commit: GitHub's published tag stays put
        // and the difference is reported instead.
        let moved = fixture.commit("two");
        git(&fixture.work, &["tag", "-f", "v1"]);
        fixture.push(&fixture.forge, &["--force", "main", "v1"]);
        let again = mirror.push_tags("git", &fixture.forge, "acme", "demo", Some(&["v1".into()]));
        assert!(again.pushed.is_empty(), "nothing is forced: {again:?}");
        assert_eq!(again.drift.len(), 1, "{again:?}");
        assert_eq!(again.drift[0].tag, "v1");
        assert_eq!(again.drift[0].forge_oid.as_deref(), Some(moved.as_str()));
        assert_eq!(
            fixture.github_ref("refs/tags/v1").as_deref(),
            Some(published.as_str()),
            "the GitHub tag did not move"
        );
    }

    #[test]
    fn an_excluded_tag_is_never_pushed_or_reported() {
        if !git_available() {
            return;
        }
        let fixture = Fixture::seed("tag-exclude");
        fixture.push(&fixture.github, &["main"]);
        git(&fixture.work, &["tag", "v9.9.9"]);
        git(&fixture.work, &["tag", "ci-0123abc"]);
        fixture.push(&fixture.forge, &["v9.9.9", "ci-0123abc"]);
        let mirror = fixture.mirror_excluding(&["v*"]);

        let report = mirror
            .reconcile("git", &fixture.forge, "acme", "demo")
            .expect("enrolled repo reconciles");
        assert!(
            !report.tags.pushed.iter().any(|tag| tag.starts_with('v')),
            "{report:?}"
        );
        assert!(
            report.tags.pushed.contains(&"ci-0123abc".to_string()),
            "{report:?}"
        );
        assert!(fixture.github_ref("refs/tags/v9.9.9").is_none());
        let named = mirror.push_tags(
            "git",
            &fixture.forge,
            "acme",
            "demo",
            Some(&["v9.9.9".into()]),
        );
        assert!(
            named.pushed.is_empty() && named.drift.is_empty(),
            "{named:?}"
        );

        // A release tag a person pushed to GitHub is not drift either.
        fixture.push(&fixture.github, &["v9.9.9"]);
        git(&fixture.work, &["tag", "v9.9.10"]);
        fixture.push(&fixture.github, &["v9.9.10"]);
        let again = mirror
            .reconcile("git", &fixture.forge, "acme", "demo")
            .expect("enrolled repo reconciles");
        assert!(again.tags.drift.is_empty(), "{again:?}");
    }

    #[test]
    fn tag_patterns_match_whole_names() {
        let v = vec!["v*".to_string(), "release-*-final".to_string()];
        assert!(tag_excluded(&v, "v1.7.2"));
        assert!(tag_excluded(&v, "release-2026-final"));
        assert!(!tag_excluded(&v, "ci-v1"));
        assert!(!tag_excluded(&v, "release-2026"));
        assert!(!tag_excluded(&[], "v1"));
        assert!(tag_excluded(&["exact".to_string()], "exact"));
        assert!(!tag_excluded(&["exact".to_string()], "exactly"));
    }

    #[test]
    fn reconcile_fast_forwards_a_lagging_mirror_and_pushes_its_new_tags() {
        if !git_available() {
            return;
        }
        let fixture = Fixture::seed("reconcile-behind");
        fixture.push(&fixture.github, &["main"]);
        let ahead = fixture.commit("two");
        git(&fixture.work, &["tag", "v2"]);
        fixture.push(&fixture.forge, &["main", "v2"]);

        let report = fixture
            .mirror()
            .reconcile("git", &fixture.forge, "acme", "demo")
            .expect("enrolled repo reconciles");
        assert_eq!(report.state, MirrorSync::InSync);
        assert!(report.caught_up, "the lagging mirror was caught up");
        assert!(!report.needs_a_person(), "{report:?}");
        assert_eq!(
            fixture.github_ref("refs/heads/main").as_deref(),
            Some(ahead.as_str())
        );
        assert_eq!(report.tags.pushed, vec!["v1".to_string(), "v2".to_string()]);
    }

    #[test]
    fn reconcile_alarms_and_forces_nothing_when_github_is_ahead() {
        if !git_available() {
            return;
        }
        let fixture = Fixture::seed("reconcile-ahead");
        // Somebody merged on GitHub: it holds a commit the forge never saw.
        let only_on_github = fixture.commit("merged on github");
        fixture.push(&fixture.github, &["main"]);
        git(&fixture.work, &["reset", "--hard", "HEAD~1"]);

        let report = fixture
            .mirror()
            .reconcile("git", &fixture.forge, "acme", "demo")
            .expect("enrolled repo reconciles");
        assert_eq!(report.state, MirrorSync::Ahead);
        assert!(!report.caught_up, "nothing was pushed");
        assert!(report.needs_a_person());
        assert_eq!(report.github_only_commits, vec![only_on_github.clone()]);
        assert_eq!(
            fixture.github_ref("refs/heads/main").as_deref(),
            Some(only_on_github.as_str()),
            "GitHub main was not rewound or forced"
        );
    }

    #[test]
    fn reconcile_reports_a_diverged_mirror_without_touching_it() {
        if !git_available() {
            return;
        }
        let fixture = Fixture::seed("reconcile-diverged");
        let github_only = fixture.commit("github side");
        fixture.push(&fixture.github, &["main"]);
        git(&fixture.work, &["reset", "--hard", "HEAD~1"]);
        fixture.commit("forge side");
        fixture.push(&fixture.forge, &["--force", "main"]);

        let report = fixture
            .mirror()
            .reconcile("git", &fixture.forge, "acme", "demo")
            .expect("enrolled repo reconciles");
        assert_eq!(report.state, MirrorSync::Diverged);
        assert!(!report.caught_up);
        assert_eq!(report.github_only_commits, vec![github_only.clone()]);
        assert_eq!(
            fixture.github_ref("refs/heads/main").as_deref(),
            Some(github_only.as_str()),
            "a diverged mirror is left exactly as it was"
        );
    }

    #[test]
    fn an_unenrolled_repository_reconciles_nothing() {
        let mirror = GithubMirror::default();
        assert!(
            mirror
                .reconcile("git", Path::new("/nonexistent"), "acme", "demo")
                .is_none(),
            "the kill switch and an empty manifest leave every repo alone"
        );
    }
}
