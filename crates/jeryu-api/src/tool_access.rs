//! The MCP tools this forge implements, and the grant each one asks of its
//! caller.
//!
//! Capability discovery, the GitHub-compatible edge's repair hints and the
//! `/mcp` backend all answer from this one table, so no surface can steer a
//! caller at a tool the backend would refuse as `tool_unavailable`, nor at one
//! the caller's own principal may not use. A tool absent from [`IMPLEMENTED`]
//! has no durable execution adapter here: naming it in guidance dead-ends the
//! agent that follows the advice.
//!
//! The table lives outside the `web` feature because the GitHub edge compiles
//! without it and serves the same guidance.

/// The namespace every MCP tool name carries on the wire.
pub const TOOL_PREFIX: &str = "jeryu.";

/// What a dispatched MCP tool asks of its caller, mirroring the `/api/v1`
/// route that serves the same data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolAccess {
    /// Global admin, like the `/api/v1/control-plane/*` routes.
    Admin,
    /// Read access to the repository named in the call, admins included.
    RepoRead,
    /// Any authenticated account.
    Account,
}

impl ToolAccess {
    /// The machine-readable name capability discovery publishes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::RepoRead => "repo_read",
            Self::Account => "account",
        }
    }
}

/// Who is asking. An agent token acts as its bot: never a forge admin, and
/// repository-scoped tools reach only the repositories the bot is granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Principal {
    /// No credential. Discovery answers; no tool call does.
    Anonymous,
    /// A scoped agent token (`jat_`), or the bot MCP mount.
    Agent,
    /// An authenticated account without the admin role.
    User,
    /// An authenticated account with the admin role.
    Admin,
}

impl Principal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Agent => "agent",
            Self::User => "user",
            Self::Admin => "admin",
        }
    }

    pub fn authenticated(self) -> bool {
        !matches!(self, Self::Anonymous)
    }
}

/// Every tool this forge dispatches, named without [`TOOL_PREFIX`], with the
/// grant it asks for. The web MCP backend dispatches exactly these names, and
/// `mcp_backend::the_registry_matches_what_the_backend_dispatches` keeps the
/// two from drifting.
pub const IMPLEMENTED: &[(&str, ToolAccess)] = &[
    // Control-plane and live-status reads: `/api/v1/control-plane/*`.
    ("control_plane.status", ToolAccess::Admin),
    ("control_plane.priorities", ToolAccess::Admin),
    ("repo_graph.clusters", ToolAccess::Admin),
    ("repo_graph.query", ToolAccess::Admin),
    ("remote.status", ToolAccess::Admin),
    ("artifacts.latest", ToolAccess::Admin),
    ("runner_fabric.status", ToolAccess::Admin),
    ("get_system_snapshot", ToolAccess::Admin),
    ("get_ci_run_jobs", ToolAccess::Admin),
    ("get_ci_bottlenecks", ToolAccess::Admin),
    ("explain_blockers", ToolAccess::Admin),
    ("plan_validation", ToolAccess::Admin),
    // Agent runs: `/api/v1/agent-runs/*`.
    ("agent_work.start", ToolAccess::Admin),
    ("agent_work.status", ToolAccess::Admin),
    ("agent_work.control", ToolAccess::Admin),
    ("agent_work.events", ToolAccess::Admin),
    ("agent_work.tail", ToolAccess::Admin),
    ("agent_work.export_pr", ToolAccess::Admin),
    // Tool-build and tool-finder: `/api/v1/codegraph/*`, `/api/v1/tool-finder/*`.
    ("codegraph.tool_build.status", ToolAccess::Admin),
    ("codegraph.tool_build.clusters", ToolAccess::Admin),
    ("codegraph.tool_build.feedback", ToolAccess::Admin),
    ("tool_finder.clusters", ToolAccess::Admin),
    ("tool_finder.scan", ToolAccess::Admin),
    ("tool_finder.dashboard", ToolAccess::Admin),
    // `/api/v1/tools/registry/summary`, open to any authenticated account.
    ("tool_registry.summary", ToolAccess::Account),
    // Code navigation names a repository, so it asks what that repository's
    // routes ask for.
    ("code.symbols.search", ToolAccess::RepoRead),
    ("code.definition", ToolAccess::RepoRead),
    ("code.impact", ToolAccess::RepoRead),
    ("code.crate.reverse_deps", ToolAccess::RepoRead),
    ("code.references", ToolAccess::RepoRead),
    ("codegraph.query", ToolAccess::RepoRead),
];

/// What the repository-scoped tools ask for beyond the grant itself, said in
/// one place because every surface that lists them has to say it.
pub const REPO_SCOPE_NOTE: &str = "A repository-scoped tool answers only for a repository this principal may read; \
     naming any other is refused, and an agent token reaches only its bot's repositories.";

/// The grant `tool` asks for, named with or without [`TOOL_PREFIX`], or `None`
/// when no adapter here runs it.
pub fn required_access(tool: &str) -> Option<ToolAccess> {
    let tool = tool.strip_prefix(TOOL_PREFIX).unwrap_or(tool);
    IMPLEMENTED
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, access)| *access)
}

/// Whether this forge dispatches `tool` at all.
pub fn is_implemented(tool: &str) -> bool {
    required_access(tool).is_some()
}

/// Whether `principal` clears the grant `access` asks for. A repository-scoped
/// tool additionally needs the named repository to be in reach, which only the
/// call itself can check ([`REPO_SCOPE_NOTE`]).
pub fn can_use(principal: Principal, access: ToolAccess) -> bool {
    match (principal, access) {
        (Principal::Anonymous, _) => false,
        (Principal::Admin, _) => true,
        (_, ToolAccess::Admin) => false,
        (_, ToolAccess::RepoRead | ToolAccess::Account) => true,
    }
}

/// Every implemented tool, prefixed, in catalog order.
pub fn implemented_tools() -> Vec<String> {
    IMPLEMENTED
        .iter()
        .map(|(name, _)| format!("{TOOL_PREFIX}{name}"))
        .collect()
}

/// The implemented tools `principal` may call, prefixed. Empty for an
/// anonymous caller: discovery answers without a credential, tool calls do not.
pub fn tools_for(principal: Principal) -> Vec<String> {
    IMPLEMENTED
        .iter()
        .filter(|(_, access)| can_use(principal, *access))
        .map(|(name, _)| format!("{TOOL_PREFIX}{name}"))
        .collect()
}

/// The tools guidance may name when it does not know who is reading it: the
/// implemented ones no principal short of admin is refused outright. Served on
/// the GitHub edge's error bodies, which carry no principal of their own.
pub fn unprivileged_tools() -> Vec<String> {
    tools_for(Principal::Agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_unique_and_prefixed_consistently() {
        let mut names: Vec<&str> = IMPLEMENTED.iter().map(|(name, _)| *name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "a tool is listed twice");
        for name in names {
            assert!(
                !name.starts_with(TOOL_PREFIX),
                "{name} is listed with its wire prefix"
            );
            assert!(is_implemented(name));
            assert!(is_implemented(&format!("{TOOL_PREFIX}{name}")));
        }
    }

    /// The tools the retired guidance named: none of them has an adapter, so
    /// no surface may recommend one.
    #[test]
    fn tools_without_an_adapter_are_not_implemented() {
        for tool in [
            "run_tests",
            "propose_patch",
            "request_merge",
            "bug_submit",
            "bug_list",
            "workcell.claim",
            "workcell.status",
            "workcell.repair_live",
            "workcell.export_pr",
            "workcell.release",
        ] {
            assert!(!is_implemented(tool), "{tool} has no adapter here");
            assert!(!is_implemented(&format!("{TOOL_PREFIX}{tool}")));
        }
    }

    #[test]
    fn an_anonymous_caller_may_use_no_tool_and_an_admin_every_one() {
        assert!(tools_for(Principal::Anonymous).is_empty());
        assert_eq!(tools_for(Principal::Admin), implemented_tools());
        assert!(!Principal::Anonymous.authenticated());
        assert!(Principal::Agent.authenticated());
    }

    /// An agent token and a plain account reach the same tools: neither is a
    /// forge admin, and both are refused every admin-only tool.
    #[test]
    fn a_scoped_agent_sees_the_non_admin_tools_only() {
        let agent = tools_for(Principal::Agent);
        assert_eq!(agent, tools_for(Principal::User));
        assert_eq!(agent, unprivileged_tools());
        assert!(agent.contains(&"jeryu.tool_registry.summary".to_string()));
        assert!(agent.contains(&"jeryu.code.definition".to_string()));
        for tool in agent {
            assert_ne!(
                required_access(&tool),
                Some(ToolAccess::Admin),
                "{tool} is admin-only"
            );
        }
        assert!(!tools_for(Principal::Agent).contains(&"jeryu.get_system_snapshot".to_string()));
    }
}
