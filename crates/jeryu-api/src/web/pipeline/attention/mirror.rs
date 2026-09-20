//! Offsite mirrors that stopped working. One item for the whole forge, however
//! many repositories it touches: the cause is almost always one host setting,
//! and eight identical rows would bury everything else in the inbox.

use chrono::{DateTime, Utc};

use super::{Draft, Hosts, Item, Severity, Shell};

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
