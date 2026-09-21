//! The closed set of `code` values an `/api/v1` error envelope can carry.
//!
//! Published at `GET /api/v1/errors` and in `docs/errors.md`. A test scans the
//! web sources and fails when a handler answers a code missing from this list.

/// One published error code.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ErrorCode {
    pub code: &'static str,
    /// The status this code usually answers with.
    pub status: u16,
    pub summary: &'static str,
}

const fn entry(code: &'static str, status: u16, summary: &'static str) -> ErrorCode {
    ErrorCode {
        code,
        status,
        summary,
    }
}

/// Every code, sorted by name.
pub(crate) const ERROR_CODES: &[ErrorCode] = &[
    entry(
        "agent_run_control_closed",
        409,
        "the agent run no longer accepts control messages",
    ),
    entry(
        "agent_run_control_unavailable",
        409,
        "the agent run has no live control channel",
    ),
    entry(
        "agent_run_control_unsupported",
        422,
        "the agent run driver does not support this control",
    ),
    entry(
        "agent_run_export_slice_denied",
        403,
        "the export touches paths outside the agent run slice",
    ),
    entry(
        "agent_run_export_source_unavailable",
        424,
        "the agent run workspace to export is unavailable",
    ),
    entry("agent_run_finished", 409, "the agent run already finished"),
    entry(
        "agent_run_invalid_control",
        422,
        "the control message is not valid for an agent run",
    ),
    entry(
        "agent_run_invalid_request",
        422,
        "the agent run request body failed validation",
    ),
    entry(
        "agent_run_not_finished",
        409,
        "the agent run has not finished yet",
    ),
    entry(
        "agent_run_path_denied",
        403,
        "the agent run path is outside the allowed slice",
    ),
    entry(
        "agent_run_workcell_state_denied",
        409,
        "the workcell state does not allow this agent run step",
    ),
    entry(
        "api_route_not_found",
        404,
        "no API route matches the request path",
    ),
    entry(
        "attention_collect_failed",
        500,
        "the attention inbox could not be collected",
    ),
    entry(
        "bad_head",
        409,
        "the pull request head does not match the queued head",
    ),
    entry(
        "bad_request",
        422,
        "the request was rejected before it reached a handler",
    ),
    entry(
        "blob_too_large",
        413,
        "the requested blob is too large to render",
    ),
    entry("ci_run_id_required", 422, "a CI run id is required"),
    entry(
        "codegraph_index_failed",
        500,
        "the codegraph index could not be built",
    ),
    entry(
        "codegraph_invalid_request",
        422,
        "the codegraph query failed validation",
    ),
    entry(
        "codegraph_materialize_failed",
        500,
        "the repository could not be materialized for codegraph",
    ),
    entry("codegraph_query_failed", 500, "the codegraph query failed"),
    entry(
        "conflict",
        409,
        "the request conflicts with the current state",
    ),
    entry(
        "csrf_required",
        403,
        "a session-authenticated write needs a valid CSRF token",
    ),
    entry(
        "events_invalid_query",
        422,
        "the pipeline event query failed validation",
    ),
    entry(
        "events_invalid_request",
        422,
        "the pipeline event body failed validation",
    ),
    entry(
        "events_reporter_required",
        403,
        "only an admin or a JERYU_EVENT_REPORTERS login may post events",
    ),
    entry(
        "events_store_failed",
        500,
        "the pipeline event store failed",
    ),
    entry("forbidden", 403, "the account may not perform this request"),
    entry(
        "forge_branch_protection",
        405,
        "branch protection blocks this operation",
    ),
    entry(
        "forge_conflict",
        409,
        "the forge state conflicts with this operation",
    ),
    entry("forge_not_found", 404, "the forge entity was not found"),
    entry(
        "forge_repository_archived",
        409,
        "the repository is archived",
    ),
    entry("forge_storage", 500, "the forge storage backend failed"),
    entry(
        "forge_validation",
        422,
        "the forge rejected the request fields",
    ),
    entry("git_error", 500, "a git operation failed"),
    entry(
        "git_source_failed",
        500,
        "reading repository source from git failed",
    ),
    entry(
        "internal_error",
        500,
        "the server failed while handling the request",
    ),
    entry("invalid_branch", 422, "the branch name is not valid"),
    entry("invalid_compare", 422, "the compare range is not valid"),
    entry(
        "invalid_input",
        422,
        "the request failed boundary validation",
    ),
    entry(
        "invalid_json_body",
        422,
        "the request body is not valid JSON for this route",
    ),
    entry(
        "invalid_page_parameter",
        422,
        "limit, per_page or page is outside the accepted range",
    ),
    entry(
        "invalid_path_parameter",
        422,
        "a path parameter has the wrong shape for this route",
    ),
    entry(
        "invalid_query",
        422,
        "the query string does not match this route",
    ),
    entry("invalid_ref", 422, "the git ref is not valid"),
    entry(
        "invalid_session_id",
        422,
        "an agent_id or run_id has a character a ref may not carry",
    ),
    entry(
        "merge_blocked",
        409,
        "merge policy blocks this pull request",
    ),
    entry("merge_failed", 500, "the merge failed"),
    entry(
        "merge_gate_blocked",
        409,
        "the merge gate has not passed for the current head",
    ),
    entry(
        "merge_passport_stale",
        409,
        "the merge passport is for an older head",
    ),
    entry(
        "merge_sha_stale",
        409,
        "the expected head sha is no longer the pull request head",
    ),
    entry(
        "merge_unprocessable",
        422,
        "the merge request could not be processed",
    ),
    entry(
        "method_not_allowed",
        405,
        "the route exists but not for this HTTP method",
    ),
    entry("not_a_file", 422, "the path names a directory, not a file"),
    entry(
        "not_acceptable",
        406,
        "the Accept header excludes the JSON this route answers",
    ),
    entry("not_found", 404, "the requested entity was not found"),
    entry("not_open", 409, "the pull request is not open"),
    entry(
        "not_queued",
        409,
        "the pull request is not in the merge queue",
    ),
    entry(
        "password_change_required",
        403,
        "the account must change its password first",
    ),
    entry(
        "payload_too_large",
        413,
        "the request body is larger than this route accepts",
    ),
    entry(
        "permission_denied",
        403,
        "the account lacks the permission this request needs",
    ),
    entry(
        "pins_collect_failed",
        500,
        "the deploy pins could not be collected",
    ),
    entry(
        "policy_denied",
        403,
        "a repository policy blocks this request",
    ),
    entry(
        "pull_approve_invalid_request",
        422,
        "the approval body failed validation",
    ),
    entry(
        "pull_comment_body_required",
        422,
        "a pull request comment needs a body",
    ),
    entry(
        "pull_comment_invalid_request",
        422,
        "the pull request comment body failed validation",
    ),
    entry(
        "pull_merge_invalid_request",
        422,
        "the merge body failed validation",
    ),
    entry(
        "pull_request_serialize_failed",
        500,
        "the pull request could not be serialized",
    ),
    entry(
        "pull_review_invalid_request",
        422,
        "the review body failed validation",
    ),
    entry(
        "pull_self_approval_forbidden",
        403,
        "an author may not approve their own pull request",
    ),
    entry(
        "queue_conflict",
        409,
        "the queued change conflicts with the queue base",
    ),
    entry(
        "queue_git_error",
        500,
        "a git operation on the merge queue failed",
    ),
    entry(
        "queue_merge_commits",
        409,
        "the queued branch contains merge commits",
    ),
    entry(
        "queue_mismatch",
        409,
        "the queue replay does not match the queued tree",
    ),
    entry("rate_limited", 429, "too many attempts; wait and retry"),
    entry("repository_archived", 409, "the repository is archived"),
    entry("request_failed", 400, "the request failed"),
    entry(
        "runner_policy_denied",
        422,
        "the runner policy denies this trust tier on the chosen runner",
    ),
    entry(
        "serialization_failed",
        500,
        "the response could not be serialized",
    ),
    entry(
        "service_unavailable",
        503,
        "the service is temporarily unavailable",
    ),
    entry(
        "session_already_published",
        409,
        "the session was already published",
    ),
    entry(
        "session_invalid_request",
        422,
        "the session request body failed validation",
    ),
    entry(
        "session_publish_source_unavailable",
        424,
        "the session workspace to publish is unavailable",
    ),
    entry(
        "session_ref_conflict",
        409,
        "the session branch update conflicts",
    ),
    entry(
        "session_ref_failed",
        500,
        "the session branch could not be updated",
    ),
    entry(
        "session_ref_invalid",
        422,
        "the session branch or path is not valid",
    ),
    entry(
        "session_ref_protected",
        403,
        "the session may not write a protected ref",
    ),
    entry(
        "session_repo_malformed",
        422,
        "the session repository is malformed",
    ),
    entry(
        "session_repo_not_found",
        404,
        "the session repository has no bare repo",
    ),
    entry(
        "session_repo_uninitialized",
        424,
        "the repository has no default branch to cut a session from",
    ),
    entry(
        "session_rotation_failed",
        500,
        "the session could not be rotated",
    ),
    entry(
        "session_runner_rejected",
        422,
        "the runner rejected the agent session plan",
    ),
    entry(
        "shift_branch_not_found",
        404,
        "the shift branch was not found",
    ),
    entry(
        "shift_family_not_found",
        404,
        "the shift family was not found",
    ),
    entry(
        "shift_heartbeat_store_failed",
        500,
        "the shift heartbeat store failed",
    ),
    entry(
        "shift_invalid_request",
        422,
        "the shift request body failed validation",
    ),
    entry(
        "shift_queue_read_failed",
        500,
        "the shift queue could not be read",
    ),
    entry(
        "shift_queue_write_failed",
        500,
        "the shift queue could not be written",
    ),
    entry("shift_todo_not_found", 404, "the shift todo was not found"),
    entry("storage_failed", 500, "the storage backend failed"),
    entry(
        "tool_build_feedback_reason_required",
        422,
        "tool-build feedback needs a reason",
    ),
    entry(
        "tool_build_invalid_request",
        422,
        "the tool-build request failed validation",
    ),
    entry(
        "tool_build_store_unavailable",
        424,
        "the tool-build codegraph store is unavailable",
    ),
    entry(
        "tool_finder_cluster_not_found",
        404,
        "the tool-finder cluster was not found",
    ),
    entry(
        "tool_finder_invalid_request",
        422,
        "the tool-finder request failed validation",
    ),
    entry(
        "tool_finder_not_configured",
        424,
        "the tool finder is not configured",
    ),
    entry(
        "tool_finder_registry_unavailable",
        424,
        "the tool registry is unavailable",
    ),
    entry(
        "tool_finder_registry_write_failed",
        500,
        "the tool registry could not be written",
    ),
    entry(
        "tool_finder_scan_running",
        409,
        "a tool-finder scan is already running",
    ),
    entry(
        "tool_finder_source_failed",
        500,
        "the tool-finder source could not be read",
    ),
    entry(
        "tool_finder_store_unavailable",
        424,
        "the tool-finder store is unavailable",
    ),
    entry(
        "tool_finder_tool_id_taken",
        409,
        "the tool id is already registered",
    ),
    entry(
        "tool_proposal_invalid_request",
        422,
        "the tool proposal decision failed validation",
    ),
    entry(
        "tool_proposal_not_found",
        404,
        "the tool proposal was not found",
    ),
    entry(
        "tool_proposal_not_proposed",
        409,
        "the tool is not in the proposed state",
    ),
    entry(
        "tool_proposal_registry_unavailable",
        424,
        "the tool registry is unavailable",
    ),
    entry(
        "tool_proposal_registry_write_failed",
        500,
        "the tool registry could not be written",
    ),
    entry("unauthorized", 401, "the request needs a login or token"),
    entry(
        "unsupported_media_type",
        415,
        "the request body needs Content-Type: application/json",
    ),
    entry(
        "workcell_branch_budget_denied",
        409,
        "the workcell branch budget is spent",
    ),
    entry(
        "workcell_claim_denied",
        409,
        "the workcell could not be claimed",
    ),
    entry(
        "workcell_delete_denied",
        403,
        "workcells may not delete branches",
    ),
    entry("workcell_epoch_fenced", 409, "the workcell epoch is stale"),
    entry(
        "workcell_export_slice_denied",
        403,
        "the export touches paths outside the workcell slice",
    ),
    entry(
        "workcell_id_mismatch",
        422,
        "the workcell id in the body does not match the path",
    ),
    entry(
        "workcell_invalid_request",
        422,
        "the workcell request body failed validation",
    ),
    entry("workcell_merge_denied", 403, "workcells may not merge"),
    entry(
        "workcell_repair_state_denied",
        409,
        "the workcell state does not allow repair",
    ),
    entry(
        "workcell_request_denied",
        400,
        "the workcell controller denied the request",
    ),
    entry(
        "workcell_run_join_failed",
        500,
        "the workcell run could not be joined",
    ),
    entry(
        "workcell_run_path_denied",
        403,
        "the workcell run path is outside the repo slice",
    ),
    entry(
        "workcell_run_policy_denied",
        422,
        "the workcell run policy denied the command",
    ),
    entry(
        "workcell_run_sandbox_unavailable",
        424,
        "the host sandbox for the workcell run is unavailable",
    ),
    entry(
        "workcell_run_supervision_failed",
        500,
        "the workcell run supervision failed",
    ),
    entry(
        "workcell_run_workspace_denied",
        422,
        "the workcell run workspace was denied",
    ),
    entry(
        "workcell_startup_rebase_failed",
        409,
        "the workcell startup rebase failed",
    ),
    entry(
        "workcell_tar_path_denied",
        422,
        "the workcell archive contains a denied path",
    ),
];

pub(crate) fn lookup(code: &str) -> Option<&'static ErrorCode> {
    ERROR_CODES.iter().find(|entry| entry.code == code)
}

/// The code a bare status answers when nothing more specific is known.
pub(crate) fn for_status(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        405 => "method_not_allowed",
        406 => "not_acceptable",
        409 => "conflict",
        413 => "payload_too_large",
        415 => "unsupported_media_type",
        422 => "invalid_input",
        429 => "rate_limited",
        503 => "service_unavailable",
        500..=599 => "internal_error",
        _ => "request_failed",
    }
}
