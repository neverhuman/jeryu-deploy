use jeryu_core::{AccountSummary, UserRole};
use jeryu_readmodel::contracts::WebFeatureFlags;

pub(super) fn permissions() -> Vec<String> {
    [
        "audit.read",
        "agents.grant",
        "agents.read",
        "agents.write",
        "agents.manage",
        "branch.create",
        "branch.delete",
        "ci.read",
        "ci.write",
        "code.read",
        "code.write",
        "issue.read",
        "issue.write",
        "pr.approve",
        "pr.comment",
        "pr.merge",
        "pr.read",
        "pr.review",
        "pr.write",
        "repo.create",
        "repo.delete",
        "repo.manage",
        "repo.read",
        "repo.write",
        "secrets.metadata",
        "secrets.write",
        "settings.read",
        "settings.write",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Bootstrap feature flags, decided per viewer.
///
/// Every flag here gates a surface that exists on this server; none of them is
/// a placeholder for unwritten code. The write-side flags answer "may *this*
/// viewer use it", so they follow the same checks the request gate applies:
///
/// * `repo_create` — `POST /repos` on the GitHub edge creates a repository for
///   an owner. Offered to global admins only: repository namespaces are a
///   forge-wide decision, and per-repo grants say nothing about creating new
///   ones.
/// * `settings_write`, `merge_write`, `agents` — the repository settings PATCH,
///   the merge and merge-queue routes and the agent-session routes are all
///   repo-scoped writes, so the flag is on for a viewer with write access to at
///   least one repository they can see.
/// * `markdown_html`, `mcp`, `workcells` — read-side surfaces this server
///   always serves.
///
/// [`FEATURE_FLAG_NOTES`] states the same decisions at `/.jeryu/capabilities`
/// so an operator seeing a flag off can tell policy from a missing grant.
pub(super) fn feature_flags(
    state: &super::WebState,
    account: Option<&AccountSummary>,
) -> WebFeatureFlags {
    // No account is the in-process local operator (the CLI-side bootstrap),
    // which is not subject to per-repo grants.
    let admin = account.is_none_or(|account| account.role == UserRole::Admin);
    let repo_writer = admin || account.is_some_and(|account| writes_any_repo(state, account));
    WebFeatureFlags {
        repo_create: admin,
        settings_write: repo_writer,
        merge_write: repo_writer,
        markdown_html: true,
        agents: repo_writer,
        mcp: true,
        workcells: true,
    }
}

/// Operator-facing note per flag: what it gates, and what turns it on.
pub(super) const FEATURE_FLAG_NOTES: [(&str, &str); 7] = [
    (
        "repo_create",
        "POST /repos on the GitHub edge; offered to global admins only, because creating a repository namespace is a forge-wide decision.",
    ),
    (
        "settings_write",
        "PATCH /api/v1/repos/{id}; on for a viewer with write access to at least one visible repository.",
    ),
    (
        "merge_write",
        "pull-request merge and merge-queue routes; on for a viewer with write access to at least one visible repository.",
    ),
    ("markdown_html", "POST /api/v1/markdown/render; always on."),
    (
        "agents",
        "agent-session and agent-run control routes; on for a viewer with write access to at least one visible repository.",
    ),
    ("mcp", "the /mcp endpoint and jeryu.* tools; always on."),
    (
        "workcells",
        "the workcell routes and their websocket scopes; always on.",
    ),
];

/// Whether the viewer may write to any repository they can see. The bootstrap
/// flags are forge-wide, so one writable repository is enough to render the
/// write surfaces; each request is still authorized per repository.
fn writes_any_repo(state: &super::WebState, account: &AccountSummary) -> bool {
    state.core.list_repositories(None).iter().any(|repo| {
        state
            .core
            .user_can_write_repo(&account.login, &repo.owner, &repo.name)
    })
}
