# Error Repair Surface

Jeryu domain errors expose an `AgentRepairHint` with five required fields:
`purpose`, `reason`, `common_fixes`, `docs_url`, and `repair_hint`.
Agents should route failures from this typed surface instead of scraping display
strings.

Every HTTP and MCP response carries a bounded `x-request-id`. Include that value
with the typed error fields when correlating a failure; caller-supplied IDs must
use only ASCII letters, digits, `-`, `_`, `.`, or `:` and be at most 128 bytes.

## API Error Envelope

Every error under `/api/v1` (and any unknown `/api/*` path) answers one JSON
shape with `Content-Type: application/json`, including the rejections the HTTP
framework produces itself (malformed JSON, a missing `Content-Type`, a path
parameter of the wrong type, a method the route does not take, a missing login):

```json
{
  "code": "invalid_json_body",
  "message": "the request body is not valid JSON for this route",
  "reason": "expected value at line 1 column 1",
  "purpose": "complete a jeryu API request",
  "common_fixes": ["send a body that parses as JSON"],
  "repair_hint": "look the code up at GET /api/v1/errors, fix the request it names, and retry",
  "docs_url": "docs/errors.md"
}
```

Route on `code`; the other fields are for people and repair loops. Some routes
add fields of their own (for example `details` on pull-request merges), but the
seven above are always present. A 405 keeps its `Allow` header. The GitHub-shaped
edge (`/repos/...`, `/api/v3/...`) keeps GitHub's own error shape.

`GET /api/v1/errors` (no login needed) returns the closed set of codes below as
`{"schema": "jeryu.api.errors.v1", "envelope_fields": [...], "codes": [{"code", "status", "summary"}]}`.
`status` is the status a code usually answers with; route on `code`, not on it.

| code | status | summary |
| --- | --- | --- |
| `agent_run_control_closed` | 409 | the agent run no longer accepts control messages |
| `agent_run_control_unavailable` | 409 | the agent run has no live control channel |
| `agent_run_control_unsupported` | 422 | the agent run driver does not support this control |
| `agent_run_export_slice_denied` | 403 | the export touches paths outside the agent run slice |
| `agent_run_export_source_unavailable` | 424 | the agent run workspace to export is unavailable |
| `agent_run_finished` | 409 | the agent run already finished |
| `agent_run_invalid_control` | 422 | the control message is not valid for an agent run |
| `agent_run_invalid_request` | 422 | the agent run request body failed validation |
| `agent_run_not_finished` | 409 | the agent run has not finished yet |
| `agent_run_path_denied` | 403 | the agent run path is outside the allowed slice |
| `agent_run_workcell_state_denied` | 409 | the workcell state does not allow this agent run step |
| `api_route_not_found` | 404 | no API route matches the request path |
| `attention_collect_failed` | 500 | the attention inbox could not be collected |
| `bad_head` | 409 | the pull request head does not match the queued head |
| `bad_request` | 400 | the request was rejected before it reached a handler |
| `blob_too_large` | 413 | the requested blob is too large to render |
| `ci_run_id_required` | 422 | a CI run id is required |
| `codegraph_index_failed` | 500 | the codegraph index could not be built |
| `codegraph_invalid_request` | 422 | the codegraph query failed validation |
| `codegraph_materialize_failed` | 500 | the repository could not be materialized for codegraph |
| `codegraph_query_failed` | 500 | the codegraph query failed |
| `conflict` | 409 | the request conflicts with the current state |
| `csrf_required` | 403 | a session-authenticated write needs a valid CSRF token |
| `events_invalid_query` | 422 | the pipeline event query failed validation |
| `events_invalid_request` | 422 | the pipeline event body failed validation |
| `events_reporter_required` | 403 | only an admin or a JERYU_EVENT_REPORTERS login may post events |
| `events_store_failed` | 500 | the pipeline event store failed |
| `forbidden` | 403 | the account may not perform this request |
| `forge_branch_protection` | 405 | branch protection blocks this operation |
| `forge_conflict` | 409 | the forge state conflicts with this operation |
| `forge_not_found` | 404 | the forge entity was not found |
| `forge_repository_archived` | 409 | the repository is archived |
| `forge_storage` | 500 | the forge storage backend failed |
| `forge_validation` | 422 | the forge rejected the request fields |
| `git_error` | 500 | a git operation failed |
| `git_source_failed` | 500 | reading repository source from git failed |
| `internal_error` | 500 | the server failed while handling the request |
| `invalid_branch` | 422 | the branch name is not valid |
| `invalid_compare` | 422 | the compare range is not valid |
| `invalid_input` | 422 | the request failed boundary validation |
| `invalid_json_body` | 400 | the request body is not valid JSON for this route |
| `invalid_path_parameter` | 400 | a path parameter has the wrong shape for this route |
| `invalid_query` | 400 | the query string does not match this route |
| `invalid_ref` | 422 | the git ref is not valid |
| `invalid_session_id` | 422 | an agent_id or run_id has a character a ref may not carry |
| `merge_blocked` | 409 | merge policy blocks this pull request |
| `merge_failed` | 500 | the merge failed |
| `merge_gate_blocked` | 409 | the merge gate has not passed for the current head |
| `merge_passport_stale` | 409 | the merge passport is for an older head |
| `merge_sha_stale` | 409 | the expected head sha is no longer the pull request head |
| `merge_unprocessable` | 422 | the merge request could not be processed |
| `method_not_allowed` | 405 | the route exists but not for this HTTP method |
| `not_a_file` | 422 | the path names a directory, not a file |
| `not_found` | 404 | the requested entity was not found |
| `not_open` | 409 | the pull request is not open |
| `not_queued` | 409 | the pull request is not in the merge queue |
| `password_change_required` | 403 | the account must change its password first |
| `payload_too_large` | 413 | the request body is larger than this route accepts |
| `permission_denied` | 403 | the account lacks the permission this request needs |
| `pins_collect_failed` | 500 | the deploy pins could not be collected |
| `policy_denied` | 403 | a repository policy blocks this request |
| `pull_approve_invalid_request` | 422 | the approval body failed validation |
| `pull_comment_body_required` | 422 | a pull request comment needs a body |
| `pull_comment_invalid_request` | 422 | the pull request comment body failed validation |
| `pull_merge_invalid_request` | 422 | the merge body failed validation |
| `pull_request_serialize_failed` | 500 | the pull request could not be serialized |
| `pull_review_invalid_request` | 422 | the review body failed validation |
| `pull_self_approval_forbidden` | 403 | an author may not approve their own pull request |
| `queue_conflict` | 409 | the queued change conflicts with the queue base |
| `queue_git_error` | 500 | a git operation on the merge queue failed |
| `queue_merge_commits` | 409 | the queued branch contains merge commits |
| `queue_mismatch` | 409 | the queue replay does not match the queued tree |
| `rate_limited` | 429 | too many attempts; wait and retry |
| `repository_archived` | 409 | the repository is archived |
| `request_failed` | 400 | the request failed |
| `runner_policy_denied` | 422 | the runner policy denies this trust tier on the chosen runner |
| `serialization_failed` | 500 | the response could not be serialized |
| `service_unavailable` | 503 | the service is temporarily unavailable |
| `session_already_published` | 409 | the session was already published |
| `session_invalid_request` | 422 | the session request body failed validation |
| `session_publish_source_unavailable` | 424 | the session workspace to publish is unavailable |
| `session_ref_conflict` | 409 | the session branch update conflicts |
| `session_ref_failed` | 500 | the session branch could not be updated |
| `session_ref_invalid` | 422 | the session branch or path is not valid |
| `session_ref_protected` | 403 | the session may not write a protected ref |
| `session_repo_malformed` | 422 | the session repository is malformed |
| `session_repo_not_found` | 404 | the session repository has no bare repo |
| `session_repo_uninitialized` | 424 | the repository has no default branch to cut a session from |
| `session_rotation_failed` | 500 | the session could not be rotated |
| `session_runner_rejected` | 422 | the runner rejected the agent session plan |
| `shift_branch_not_found` | 404 | the shift branch was not found |
| `shift_family_not_found` | 404 | the shift family was not found |
| `shift_heartbeat_store_failed` | 500 | the shift heartbeat store failed |
| `shift_invalid_request` | 422 | the shift request body failed validation |
| `shift_queue_read_failed` | 500 | the shift queue could not be read |
| `shift_queue_write_failed` | 500 | the shift queue could not be written |
| `shift_todo_not_found` | 404 | the shift todo was not found |
| `storage_failed` | 500 | the storage backend failed |
| `tool_build_feedback_reason_required` | 422 | tool-build feedback needs a reason |
| `tool_build_invalid_request` | 422 | the tool-build request failed validation |
| `tool_build_store_unavailable` | 424 | the tool-build codegraph store is unavailable |
| `tool_finder_cluster_not_found` | 404 | the tool-finder cluster was not found |
| `tool_finder_invalid_request` | 422 | the tool-finder request failed validation |
| `tool_finder_not_configured` | 424 | the tool finder is not configured |
| `tool_finder_registry_unavailable` | 424 | the tool registry is unavailable |
| `tool_finder_registry_write_failed` | 500 | the tool registry could not be written |
| `tool_finder_scan_running` | 409 | a tool-finder scan is already running |
| `tool_finder_source_failed` | 500 | the tool-finder source could not be read |
| `tool_finder_store_unavailable` | 424 | the tool-finder store is unavailable |
| `tool_finder_tool_id_taken` | 409 | the tool id is already registered |
| `tool_proposal_invalid_request` | 422 | the tool proposal decision failed validation |
| `tool_proposal_not_found` | 404 | the tool proposal was not found |
| `tool_proposal_not_proposed` | 409 | the tool is not in the proposed state |
| `tool_proposal_registry_unavailable` | 424 | the tool registry is unavailable |
| `tool_proposal_registry_write_failed` | 500 | the tool registry could not be written |
| `unauthorized` | 401 | the request needs a login or token |
| `unsupported_media_type` | 415 | the request body needs Content-Type: application/json |
| `workcell_branch_budget_denied` | 409 | the workcell branch budget is spent |
| `workcell_claim_denied` | 409 | the workcell could not be claimed |
| `workcell_delete_denied` | 403 | workcells may not delete branches |
| `workcell_epoch_fenced` | 409 | the workcell epoch is stale |
| `workcell_export_slice_denied` | 403 | the export touches paths outside the workcell slice |
| `workcell_id_mismatch` | 422 | the workcell id in the body does not match the path |
| `workcell_invalid_request` | 422 | the workcell request body failed validation |
| `workcell_merge_denied` | 403 | workcells may not merge |
| `workcell_repair_state_denied` | 409 | the workcell state does not allow repair |
| `workcell_request_denied` | 400 | the workcell controller denied the request |
| `workcell_run_join_failed` | 500 | the workcell run could not be joined |
| `workcell_run_path_denied` | 403 | the workcell run path is outside the repo slice |
| `workcell_run_policy_denied` | 422 | the workcell run policy denied the command |
| `workcell_run_sandbox_unavailable` | 424 | the host sandbox for the workcell run is unavailable |
| `workcell_run_supervision_failed` | 500 | the workcell run supervision failed |
| `workcell_run_workspace_denied` | 422 | the workcell run workspace was denied |
| `workcell_startup_rebase_failed` | 409 | the workcell startup rebase failed |
| `workcell_tar_path_denied` | 422 | the workcell archive contains a denied path |

## Not Found

The requested repository, pull request, queue entry, receipt, or other domain
entity was not present in the current read model. Verify the typed id, refresh
the read model, and rerun the owning crate test.

## Invalid Input

The request failed boundary validation before the domain operation ran. Add or
rerun the boundary test for the rejected input shape before changing policy.

## Policy Denied

A branch, proof, queue, cache, runner, or release policy intentionally blocked
the operation. Preserve the guard and supply the required proof, approval, trust
receipt, or signed witness.

## Conflict

The operation would violate merge or state consistency. Refresh base state,
recompute the witness, and retry through the queue path.

## Missing Receipt

The operation needs durable evidence before mutation. Produce the required
release, cache, scheduler, webhook, or audit receipt and rerun the mapped proof
lane.

## Missing Proof Witness

The merge path needs proof for the exact head commit and owned paths. Run the
owner/test-map proof lane and regenerate the witness before retrying merge.

## GitHub CLI Auth Steering

Jeryu does not repair a local-host GitHub CLI problem by running `gh auth login`,
`gh auth refresh`, scraping `hosts.yml`, or hunting credential stores. Configure
the host entry with `jeryu gh-setup --host <local-jeryu-url> --token-file
~/.jeryu/secrets/merge-token`, then use `/.jeryu/capabilities`, the Jeryu REST
routes, or the `jeryu.*` MCP tools for the original PR, CI, issue, or repository
task.

If `gh` reports a stale or invalid token for an existing local Jeryu host entry,
rerun `jeryu gh-setup --host <same-local-host> --token-file
~/.jeryu/secrets/merge-token`. GitHub.com auth and local Jeryu host auth are
separate; do not run `gh auth login` for Jeryu hosts. The vault bootstrap file
at `~/.jeryu/vault/bootstrap.json` contains vault bootstrap material and is not
the `gh` host repair path.

Native agent credentials are separate from the GitHub-compatible host entry.
Use `jeryu agent auth doctor <tool>` and `jeryu agent auth import --from-host
<tool>` for portable Codex, Claude, or Jekko CLI credentials.

## Workcell Control Plane

Workcell claims, heartbeats, startup rebases, tar quarantine checks, and
branch-budget enforcement are repairable failures, not silent fallbacks. The
runnerd helpers return a typed `WorkcellError` with the same five-field repair
shape used elsewhere in the product:

- `purpose`
- `reason`
- `common_fixes`
- `docs_url`
- `repair_hint`

Use the docs-linked sections in `docs/testing.md#workcells` and
`docs/boundaries.md#workcells` to repair claim, epoch, path, or merge/delete
denials.

## Agent Run Control

High-level `/api/v1/agent-runs` failures use the same typed repair shape. Common
codes include `agent_run_workcell_state_denied` for non-held/non-repairing
workcells, `workcell_epoch_fenced` for stale failed-CI repair requests,
`agent_run_path_denied` for out-of-slice repo roots or programs,
`agent_run_control_unsupported` for controls sent to pipe-mode runs, and
`agent_run_finished` for controls sent after the driver has completed.

Use `docs/workcell.md#agent-run-control-surface` and rerun
`cargo test -p jeryu-api --features web --jobs 40 agent_runs`.

## Codegraph Oracle

Codegraph query failures are typed repairable API errors. Missing repositories
return `not_found`; malformed bodies return `invalid_input`; unresolved refs
return `invalid_ref`; checkout or index failures return codegraph-specific
repair messages. Use `docs/codegraph-oracle.md` for the route contract and
`docs/testing.md#codegraph-oracle` for rerun commands.
