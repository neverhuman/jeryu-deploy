use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceState {
    Fresh,
    Missing,
    Queued,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InsightSeverity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlPlaneSnapshot {
    pub schema_version: String,
    pub generated_at: String,
    pub local_authority: LocalAuthority,
    pub summary: ControlPlaneSummary,
    pub repos: Vec<ControlRepo>,
    pub pull_requests: Vec<ControlPullRequest>,
    pub check_runs: Vec<ControlCheckRun>,
    pub workflows: Vec<ControlWorkflow>,
    pub releases: ControlReleaseSummary,
    pub artifacts: ArtifactLatestResponse,
    pub runners: RunnerFabricResponse,
    pub workcells: Value,
    pub agent_runs: Vec<Value>,
    pub codegraph: CodegraphControlSummary,
    pub tool_build: ToolBuildControlSummary,
    pub mcp: McpToolHealth,
    pub mirror: RemoteStatusResponse,
    pub priorities: Vec<PriorityInsight>,
    pub repo_graph: RepoGraphResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalAuthority {
    pub source_of_truth: String,
    pub state: EvidenceState,
    pub docs_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlPlaneSummary {
    /// Non-archived repositories: the same active set `/api/v1/repos` lists
    /// by default, so an admin sees one number in both places.
    pub repo_count: usize,
    /// Archived repositories, reported apart so they stay visible without
    /// inflating `repo_count`.
    #[serde(default)]
    pub archived_repo_count: usize,
    pub open_pr_count: usize,
    pub draft_pr_count: usize,
    pub queued_check_count: usize,
    pub running_check_count: usize,
    pub failing_check_count: usize,
    pub missing_check_pr_count: usize,
    /// Open pull requests whose checks have not reported a failure yet: none
    /// recorded, or some queued or running. Counted per pull request so a
    /// header can say "N waiting on checks" without listing them.
    #[serde(default)]
    pub waiting_check_pr_count: usize,
    /// Open pull requests with at least one failing check. `failing_check_count`
    /// counts check runs, which is a different number when a lane breaks twice.
    #[serde(default)]
    pub failing_check_pr_count: usize,
    pub priority_count: usize,
    /// The repeated causes behind `failing_check_count`, largest first, so a
    /// backlog reads as "one broken lane" or "many" without fetching every
    /// check run. Capped at [`FAILING_CAUSE_LIMIT`] entries.
    pub failing_check_causes: Vec<FailingCheckCause>,
    pub critical_priority_count: usize,
    pub high_priority_count: usize,
    pub mirror_state: EvidenceState,
    pub artifact_state: EvidenceState,
    pub runner_state: EvidenceState,
}

/// One repeated failure shape: a check name plus the conclusion it reached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FailingCheckCause {
    pub name: String,
    pub conclusion: String,
    pub count: usize,
    /// Percent of all failing check runs, rounded down.
    pub share_percent: u32,
    /// Repositories this cause appears in, capped at [`FAILING_CAUSE_REPO_LIMIT`].
    pub repos: Vec<String>,
    pub repo_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlRepo {
    pub id: String,
    pub full_name: String,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub private: bool,
    pub archived: bool,
    pub disabled: bool,
    pub open_pull_requests: usize,
    pub draft_pull_requests: usize,
    pub queued_checks: usize,
    pub running_checks: usize,
    pub failing_checks: usize,
    pub latest_head_sha: Option<String>,
    pub state: EvidenceState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlPullRequest {
    pub repo: String,
    pub number: u64,
    pub title: String,
    /// Login of the pull request's author.
    pub author: String,
    pub draft: bool,
    pub state: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub base_sha: String,
    pub mergeable: bool,
    pub mergeable_state: String,
    pub changed_files: Vec<String>,
    pub checks: CheckSummary,
    pub state_evidence: EvidenceState,
    pub source_links: Vec<SourceLink>,
    /// When the forge last touched the pull request (RFC 3339, UTC). The
    /// collection is ordered by it, newest first, within the open ones and
    /// within the finished ones. Defaulted so a payload written before the
    /// field existed still reads.
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckSummary {
    pub total: usize,
    pub queued: usize,
    pub running: usize,
    pub failing: usize,
    pub successful: usize,
    pub missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlCheckRun {
    pub id: String,
    pub repo: String,
    pub name: String,
    pub head_sha: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub details_url: Option<String>,
    pub state: EvidenceState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlWorkflow {
    pub id: String,
    pub repo: String,
    pub name: String,
    pub head_sha: String,
    pub state: EvidenceState,
    pub check_run_id: String,
    pub jobs_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlReleaseSummary {
    pub state: EvidenceState,
    pub latest_release: Option<String>,
    pub release_count: usize,
    pub reason: String,
    pub docs_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactLatestResponse {
    pub schema_version: String,
    pub state: EvidenceState,
    pub latest_build: ArtifactEvidence,
    pub latest_release: ArtifactEvidence,
    pub mirror_artifacts: ArtifactEvidence,
    pub docs_url: String,
    pub absence_is_success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactEvidence {
    pub state: EvidenceState,
    pub artifact_count: usize,
    pub reason: String,
    pub source_links: Vec<SourceLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunnerFabricResponse {
    pub schema_version: String,
    /// The forge's own build: what code serves this page.
    #[serde(default)]
    pub forge: ForgeBuild,
    pub local: RunnerLocalFabric,
    pub mirror: MirrorEvidence,
}

/// What code the forge itself runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForgeBuild {
    /// `CARGO_PKG_VERSION` of jeryu-api.
    pub version: String,
    /// The full jeryu-deploy commit this binary was built from; `null` when
    /// the build could not tell.
    pub commit: Option<String>,
    /// The jeryu-web commit pinned by the split lock at build time.
    pub web_commit: Option<String>,
}

impl ForgeBuild {
    pub(crate) fn current() -> Self {
        Self {
            version: crate::JERYU_API_VERSION.to_string(),
            commit: crate::JERYU_BUILD_COMMIT.map(str::to_string),
            web_commit: crate::JERYU_WEB_COMMIT.map(str::to_string),
        }
    }
}

/// The code a heartbeat runner says it runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunnerCodeSummary {
    pub repo: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunnerLocalFabric {
    pub state: EvidenceState,
    pub nodes: u32,
    pub online_runners: u32,
    pub offline_runners: u32,
    pub busy_runners: u32,
    pub idle_runners: u32,
    pub total_slots: u32,
    pub active_slots: u32,
    pub utilization: f64,
    pub last_updated: Option<String>,
    pub node_details: Vec<RunnerNodeSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunnerNodeSummary {
    pub runner_id: String,
    pub source: String,
    pub state: String,
    pub capacity: u32,
    pub in_flight: u32,
    pub labels: Vec<String>,
    pub classes: Vec<String>,
    pub active_task_count: u32,
    pub last_updated: Option<String>,
    pub active_tasks: Vec<RunnerTaskSummary>,
    /// The last gate this runner finished, for runners that report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<RunnerLastActivity>,
    /// Seconds of silence after which a heartbeat runner is offline
    /// (`max(180, 3 * intervalSeconds)`); absent for workcell nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline_after_seconds: Option<i64>,
    /// Reviewer only: repositories of the PRs it is reviewing or last reviewed
    /// where the merge identity has no write grant, so an approval there
    /// cannot land.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merge_grant_gaps: Vec<crate::web::merge_attempts::MergeGrantGap>,
    /// The code this runner says it runs; absent when it sent none, and on
    /// nodes that do not report by heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<RunnerCodeSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunnerLastActivity {
    pub repo: String,
    /// `null` when the work had no pull request (a staged commit).
    #[serde(default)]
    pub pr: Option<u64>,
    pub sha: String,
    pub recipe: String,
    pub conclusion: String,
    pub seconds: u64,
    pub finished_at: String,
    /// Reviewer only: the forge's answer to the last attempt to merge this PR,
    /// so an approval the merger could not land says why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_attempt: Option<crate::web::merge_attempts::MergeAttempt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunnerTaskSummary {
    pub task_id: String,
    pub job_id: String,
    pub agent_run_id: Option<String>,
    pub workcell_id: Option<String>,
    pub repo: Option<String>,
    pub label: String,
    pub program: String,
    pub state: String,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub tty_preview: RunnerTtyPreview,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunnerTtyPreview {
    pub state: EvidenceState,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodegraphControlSummary {
    pub state: EvidenceState,
    pub indexed_symbols: usize,
    pub indexed_references: usize,
    pub crate_edges: usize,
    pub indexed_files: usize,
    pub latest_index_run: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolBuildControlSummary {
    pub state: EvidenceState,
    pub cluster_count: usize,
    pub ignored_count: usize,
    pub top_clusters: Vec<ToolBuildClusterSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolBuildClusterSummary {
    pub cluster_id: String,
    pub repo_id: String,
    pub score: u64,
    pub occurrence_count: usize,
    pub file_count: usize,
    pub insight: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpToolHealth {
    pub state: EvidenceState,
    pub tool_count: usize,
    pub live_backed_tools: Vec<String>,
    pub degraded_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteStatusResponse {
    pub schema_version: String,
    pub state: EvidenceState,
    pub mirrors: Vec<MirrorEvidence>,
    pub divergence: MirrorDivergence,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MirrorEvidence {
    pub name: String,
    pub state: EvidenceState,
    pub reason: String,
    pub docs_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MirrorDivergence {
    pub state: EvidenceState,
    pub reason: String,
    pub local_default_branches: Vec<SourceLink>,
    pub mirror_default_branches: Vec<SourceLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PriorityInsight {
    pub id: String,
    pub title: String,
    pub severity: InsightSeverity,
    pub score: u32,
    pub confidence: f64,
    pub owner: String,
    pub proof_lane: String,
    pub recommended_action: String,
    pub evidence: Vec<String>,
    pub source_links: Vec<SourceLink>,
    pub state: EvidenceState,
    pub rules_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepoGraphResponse {
    pub schema_version: String,
    pub generated_at: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub clusters: Vec<GraphCluster>,
    pub insights: Vec<GraphInsight>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphNode {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub state: EvidenceState,
    pub weight: f64,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphEdge {
    pub source: String,
    pub target: String,
    pub kind: String,
    pub state: EvidenceState,
    pub weight: f64,
    /// Edge-kind detail (a `depends_on` edge carries its pin and staleness).
    /// Absent from the wire when empty, so containment edges are unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphCluster {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub state: EvidenceState,
    pub severity: InsightSeverity,
    pub node_ids: Vec<String>,
    pub insights: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphInsight {
    pub id: String,
    pub cluster_id: String,
    pub title: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PriorityQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RepoGraphQuery {
    pub repo: Option<String>,
    /// Comma-separated opt-in edge kinds; `depends_on` adds repo-to-repo
    /// dependency edges. Callers that ask for nothing get the containment
    /// graph they always got.
    pub include: Option<String>,
    pub cluster_kind: Option<String>,
    pub query: Option<String>,
    pub limit: Option<usize>,
}
