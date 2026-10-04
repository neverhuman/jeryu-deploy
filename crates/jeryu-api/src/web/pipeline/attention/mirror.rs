//! Offsite mirrors that stopped working. One item for the whole forge, however
//! many repositories it touches: the cause is almost always one host setting,
//! and eight identical rows would bury everything else in the inbox.

use chrono::{DateTime, Utc};

use super::{Draft, Hosts, Item, Severity, Shell, repo_href};

/// A configured GitHub mirror whose newest push failed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MirrorFailure {
    /// `owner/name`.
    pub repo: String,
    pub failed_at: DateTime<Utc>,
    /// When a push last succeeded, if it ever did.
    pub last_success_at: Option<DateTime<Utc>>,
    /// What git said, already scrubbed of credentials by the mirror code.
    pub reason: String,
}

/// The last sentence git printed is the useful one ("remote: Invalid username
/// or token …"); the command line before it only repeats the repository.
fn cause(reason: &str) -> &str {
    reason
        .rsplit_once("failed: ")
        .map_or(reason, |(_, tail)| tail)
        .trim()
}

pub(crate) fn mirror_items(failures: &[MirrorFailure], hosts: &Hosts) -> Vec<Item> {
    let Some(oldest) = failures.iter().min_by_key(|failure| failure.failed_at) else {
        return Vec::new();
    };
    let never = failures
        .iter()
        .filter(|failure| failure.last_success_at.is_none())
        .count();
    let mut names: Vec<&str> = failures
        .iter()
        .map(|failure| failure.repo.as_str())
        .collect();
    names.sort_unstable();
    let shown = names.iter().take(4).copied().collect::<Vec<_>>().join(", ");
    let more = names.len().saturating_sub(4);
    let mut item = Draft {
        id: "mirror-failing".to_string(),
        kind: "mirror_failing",
        severity: Severity::Action,
        title: format!(
            "The GitHub mirror is failing for {} repositor{}",
            failures.len(),
            if failures.len() == 1 { "y" } else { "ies" }
        ),
        reason: format!(
            "git says: {}. Affected: {shown}{}. {} Merges are not affected, but GitHub no longer \
             has what main has. The forge pushes through a git `insteadOf` rewrite to an SSH \
             deploy key on its host; when that rewrite or key is missing, git falls back to \
             HTTPS with a placeholder token and GitHub refuses it.",
            cause(&oldest.reason),
            if more == 0 {
                String::new()
            } else {
                format!(" and {more} more")
            },
            if never == failures.len() {
                "No push has ever succeeded from this host.".to_string()
            } else {
                format!("{never} of them have never had a successful push.")
            },
        ),
        href: "/repos".to_string(),
        label: "Set up the mirror's SSH rewrite and deploy key on the forge host",
        command: Some(Shell {
            line: "git config --global --get-regexp '^url\\..*insteadof'".to_string(),
            // `--global` reads the home of whoever runs it, and the pushes are
            // made by the forge's own user.
            run_in: format!("{}, as the user the forge runs as", hosts.forge),
        }),
    }
    .build();
    item.since = Some(
        oldest
            .failed_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    item.repo = Some(oldest.repo.clone());
    vec![item]
}

/// A repository whose GitHub mirror holds something the forge does not: commits
/// pushed or merged on GitHub, or a tag GitHub published at another commit.
/// One item per repository, unlike a failing push: each one is its own history
/// question and names its own commits.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MirrorDrift {
    /// `owner/name` on the forge.
    pub repo: String,
    pub github_slug: String,
    /// `ahead` or `diverged`; empty when only tags drifted.
    pub branch_state: Option<String>,
    pub github_head: Option<String>,
    /// Commits GitHub has and the forge does not, newest first.
    pub github_only_commits: Vec<String>,
    /// One sentence per tag the mirror refuses to move.
    pub tag_drift: Vec<String>,
    pub since: Option<DateTime<Utc>>,
}

pub(crate) fn divergence_items(drifts: &[MirrorDrift]) -> Vec<Item> {
    drifts
        .iter()
        .map(|drift| {
            let mut reason = String::new();
            if let Some(state) = drift.branch_state.as_deref() {
                reason.push_str(&format!(
                    "github.com/{} is {state}: it holds {} the forge does not ({}). ",
                    drift.github_slug,
                    if drift.github_only_commits.len() == 1 {
                        "a commit".to_string()
                    } else {
                        format!("{} commits", drift.github_only_commits.len())
                    },
                    if drift.github_only_commits.is_empty() {
                        drift
                            .github_head
                            .clone()
                            .unwrap_or_else(|| "unlisted".to_string())
                    } else {
                        drift.github_only_commits.join(", ")
                    },
                ));
            }
            if !drift.tag_drift.is_empty() {
                reason.push_str(&drift.tag_drift.join("; "));
                reason.push_str(". ");
            }
            reason.push_str(
                "The forge is the truth, so nothing was forced and nothing was deleted. Bring the \
                 work onto the forge (or decide it is not wanted) and the next reconcile goes \
                 quiet.",
            );
            let mut item = Draft {
                id: format!("mirror-diverged-{}", drift.repo),
                kind: "mirror_diverged",
                severity: Severity::Critical,
                title: format!("GitHub has work the forge does not for {}", drift.repo),
                reason,
                href: repo_href(&drift.repo),
                label: "Decide what happens to the GitHub-only work",
                command: None,
            }
            .build();
            item.since = drift
                .since
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            item.repo = Some(drift.repo.clone());
            item
        })
        .collect()
}
