# Error Repair Surface

Jeryu domain errors expose an `AgentRepairHint` with five required fields:
`purpose`, `reason`, `common_fixes`, `docs_url`, and `repair_hint`.
Agents should route failures from this typed surface instead of scraping display
strings.

Every HTTP and MCP response carries a bounded `x-request-id`. Include that value
with the typed error fields when correlating a failure; caller-supplied IDs must
use only ASCII letters, digits, `-`, `_`, `.`, or `:` and be at most 128 bytes.

## Agent Discovery

Every URL an error body advertises is served under `/api/v1`, the prefix the
edge routes:

| URL | answers |
| --- | --- |
| `GET /api/v1/capabilities` | the capability manifest: live endpoints, the `gh` command map, the installed `jeryu.*` MCP tools, and the `gh` auth policy. Also served at `/.jeryu/capabilities` for clients that learned that path first. |
| `GET /api/v1/docs` | the documentation pages this build serves |
| `GET /api/v1/docs/<page>` | one repository markdown page as `text/markdown`, named the way the repository spells it (`errors.md`, `pipeline-events.md`) |
| `GET /api/v1/docs/rest` | the GitHub-compatible edge's document: its routes, its auth and the faster typed paths |
| `GET /api/v1` | the typed route index: every mounted route with its path parameters, the query keys it reads, and what each method needs (`none`, `token` or `admin`) |
| `GET /api/v1/openapi.json` | the OpenAPI 3.1 document, generated from the mounted routes |
| `GET /api/v1/errors` | the closed set of error codes |
| `GET /api/v3` | the GitHub-compatible edge's index, `200`, listing every route it serves |

None of these needs a login: an unauthenticated refusal advertises them, so
asking for a token to read them would dead-end the agent being steered.

Every `429` carries `Retry-After` (seconds until the window resets) plus
`X-RateLimit-Limit`, `X-RateLimit-Remaining` and `X-RateLimit-Reset` (a Unix
timestamp). A metered read carries the same three headers when it succeeds, so
a polling client can slow down before it is refused.

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
  "docs_url": "/api/v1/docs/errors.md"
}
```

`docs_url` is a URL this server serves: the pages under `/api/v1/docs/` are the
repository's own markdown, embedded in the binary, so an agent can fetch what an
error advertises instead of landing on the web app's HTML. See
[Agent Discovery](#agent-discovery).

Route on `code`; the other fields are for people and repair loops. Some routes
add fields of their own (for example `details` on pull-request merges), but the
seven above are always present. A 405 keeps its `Allow` header. The GitHub-shaped
edge (`/repos/...`, `/api/v3/...`) keeps GitHub's own error shape.

`GET /api/v1/errors` (no login needed) returns the closed set of codes below as
`{"schema": "jeryu.api.errors.v1", "envelope_fields": [...], "codes": [{"code", "status", "summary"}]}`.
`status` is the status a code usually answers with; route on `code`, not on it.

| code | status | summary |
| --- | --- | --- |
| `account_not_found` | 404 | no forge account has that login |
| `agent_run_control_closed` | 409 | the agent run no longer accepts control messages |
| `agent_run_control_not_held` | 409 | another client holds the control lease for this terminal |
| `agent_run_control_unavailable` | 409 | the agent run has no live control channel |
| `agent_run_control_unsupported` | 422 | the agent run driver does not support this control |
| `agent_run_export_slice_denied` | 403 | the export touches paths outside the agent run slice |
| `agent_run_export_source_unavailable` | 424 | the agent run workspace to export is unavailable |
| `agent_run_finished` | 409 | the agent run already finished |
| `agent_run_input_invalid` | 422 | send_bytes input is not non-empty standard base64 within 64 KiB |
| `agent_run_invalid_control` | 422 | the control message is not valid for an agent run |
| `agent_run_invalid_request` | 422 | the agent run request body failed validation |
| `agent_run_not_finished` | 409 | the agent run has not finished yet |
| `agent_run_not_recorded` | 503 | the agent run could not be recorded, so it was not started |
| `agent_run_path_denied` | 403 | the agent run path is outside the allowed slice |
| `agent_run_repo_source_use_sessions` | 422 | repository agent runs start through the repo sessions route |
| `agent_run_workcell_state_denied` | 409 | the workcell state does not allow this agent run step |
| `already_exists` | 409 | the resource the request would create already exists |
| `api_route_not_found` | 404 | no API route matches the request path |
| `approval_blocked_jankurai_proof` | 409 | `jankurai/proof` on this head fails, has not run, or has not finished, and this repository is under the gate |
| `attention_collect_failed` | 500 | the attention inbox could not be collected |
| `bad_head` | 409 | the pull request head does not match the queued head |
| `bad_request` | 422 | the request was rejected before it reached a handler |
| `blob_too_large` | 413 | the requested blob is too large to render |
| `ci_run_id_required` | 422 | a CI run id is required |
| `claim_live` | 409 | the todo's claim lease is still live, so a worker is running it |
| `codegraph_index_failed` | 500 | the codegraph index could not be built |
| `codegraph_invalid_request` | 422 | the codegraph query failed validation |
| `codegraph_materialize_failed` | 500 | the repository could not be materialized for codegraph |
| `codegraph_query_failed` | 500 | the codegraph query failed |
| `conflict` | 409 | the request conflicts with the current state |
| `creation_failed` | 500 | the creation did not complete; inspect the server log before retrying |
| `creation_incomplete` | 409 | an earlier creation with this key is still running or needs recovery |
| `csrf_required` | 403 | a session-authenticated write needs a valid CSRF token |
| `events_invalid_query` | 422 | the pipeline event query failed validation |
| `events_invalid_request` | 422 | the pipeline event body failed validation |
| `events_reporter_required` | 403 | only an admin or a JERYU_EVENT_REPORTERS login may post events |
| `events_store_failed` | 500 | the pipeline event store failed |
| `family_unknown` | 422 | no product family of that name: the filter would have matched nothing |
| `flow_layout_read_failed` | 500 | the heartbeat store behind the flow layout report could not be read |
| `flow_read_failed` | 500 | the flow ledger behind the Flow report could not be read |
| `forbidden` | 403 | the account may not perform this request |
| `forge_branch_protection` | 405 | branch protection blocks this operation |
| `forge_conflict` | 409 | the forge state conflicts with this operation |
| `forge_not_found` | 404 | the forge entity was not found |
| `forge_forbidden` | 403 | this identity may not do that on the repository |
| `forge_repository_archived` | 409 | the repository is archived |
| `forge_storage` | 500 | the forge storage backend failed |
| `forge_validation` | 422 | the forge rejected the request fields |
| `git_error` | 500 | a git operation failed |
| `git_source_failed` | 500 | reading repository source from git failed |
| `host_launch_denied` | 403 | managed agent or terminal execution on the host is denied |
| `idempotency_conflict` | 409 | the Idempotency-Key was used with a different request |
| `idempotency_key_in_flight` | 409 | a request with this Idempotency-Key is still running |
| `idempotency_key_reused` | 422 | the Idempotency-Key was already used for a different request |
| `idempotency_store_failed` | 500 | the Idempotency-Key store could not be read |
| `idempotent_secret_withheld` | 409 | this request already ran; its once-shown secret is not kept |
| `insufficient_scope` | 403 | the bot grant does not allow this request |
| `internal_error` | 500 | the server failed while handling the request |
| `invalid_branch` | 422 | the branch name is not valid |
| `invalid_compare` | 422 | the compare range is not valid |
| `invalid_input` | 422 | the request failed boundary validation |
| `invalid_json_body` | 422 | the request body is not valid JSON for this route |
| `invalid_page_parameter` | 422 | limit, per_page or page is outside the accepted range |
| `invalid_path_parameter` | 422 | a path parameter has the wrong shape for this route |
| `invalid_query` | 422 | the query string does not match this route |
| `invalid_ref` | 422 | the git ref is not valid |
| `invalid_request` | 422 | credentials must not be passed as query parameters |
| `invalid_session_id` | 422 | an agent_id or run_id has a character a ref may not carry |
| `invalid_token` | 401 | invalid bot credential |
| `managed_runtime_invalid` | 422 | the configured managed runtime is unknown |
| `merge_blocked` | 409 | merge policy blocks this pull request |
| `merge_failed` | 500 | the merge failed |
| `merge_gate_blocked` | 409 | the merge gate has not passed for the current head |
| `merge_passport_stale` | 409 | the merge passport is for an older head |
| `merge_sha_stale` | 409 | the expected head sha is no longer the pull request head |
| `merge_unprocessable` | 422 | the merge request could not be processed |
| `method_not_allowed` | 405 | the route exists but not for this HTTP method |
| `needs_reconciliation` | 409 | a bot operation with this key is still in progress |
| `not_a_file` | 422 | the path names a directory, not a file |
| `not_acceptable` | 406 | the Accept header excludes the JSON this route answers |
| `not_found` | 404 | the requested entity was not found |
| `not_open` | 409 | the pull request is not open |
| `not_queued` | 409 | the pull request is not in the merge queue |
| `origin_denied` | 403 | a cookie-authenticated WebSocket came from another origin |
| `password_change_required` | 403 | the account must change its password first |
| `payload_too_large` | 413 | the request body is larger than this route accepts |
| `permission_denied` | 403 | the account lacks the permission this request needs |
| `pins_collect_failed` | 500 | the deploy pins could not be collected |
| `policy_denied` | 403 | a repository policy blocks this request |
| `presence_limit` | 429 | retained sessions exceed 64 per reporter or 1024 forge-wide; reuse an existing id |
| `presence_write_budget` | 429 | presence writes exceed 120 per login per minute; reduce beat frequency |
| `presence_invalid_request` | 422 | the agent presence beat failed validation |
| `presence_repo_unknown` | 422 | the beat names a repository this forge does not host, or one the caller cannot see |
| `presence_scope_empty` | 422 | an agent presence beat must name at least one repository or family |
| `presence_scope_forbidden` | 403 | the login may not report a session on that repository or family |
| `presence_session_not_found` | 404 | no agent presence session has that id |
| `presence_session_owned` | 409 | another login registered this agent presence session |
| `presence_store_failed` | 500 | the agent presence store could not be read or written |
| `pull_approve_invalid_request` | 422 | the approval body failed validation |
| `pull_comment_body_required` | 422 | a pull request comment needs a body |
| `pull_comment_invalid_request` | 422 | the pull request comment body failed validation |
| `pull_draft_forbidden` | 403 | only the author or an admin can change a pull request's draft state |
| `pull_draft_not_open` | 409 | only an open pull request can change its draft state |
| `pull_import_failed` | 500 | the mirror pull request import did not run |
| `pull_import_github_unreachable` | 502 | the pull request could not be read from GitHub |
| `pull_import_importer_required` | 403 | only an admin or a JERYU_PULL_IMPORTERS login may deliver mirror pull request events |
| `pull_import_invalid_request` | 422 | the mirror pull request event body failed validation |
| `pull_merge_invalid_request` | 422 | the merge body failed validation |
| `pull_origin_author_forbidden` | 403 | a pull request's own author may not mark its origin |
| `pull_origin_importer_required` | 403 | only an admin or a JERYU_PULL_IMPORTERS login may mark a pull request's origin |
| `pull_origin_invalid_request` | 422 | the pull request origin body failed validation |
| `pull_request_serialize_failed` | 500 | the pull request could not be serialized |
| `pull_review_invalid_request` | 422 | the review body failed validation |
| `pull_self_approval_forbidden` | 403 | an author may not approve their own pull request |
| `queue_conflict` | 409 | the queued change conflicts with the queue base |
| `queue_git_error` | 500 | a git operation on the merge queue failed |
| `queue_merge_commits` | 409 | the queued branch contains merge commits |
| `queue_mismatch` | 409 | the queue replay does not match the queued tree |
| `rate_limited` | 429 | too many attempts; wait and retry |
| `repo_not_granted` | 403 | the bot has no grant for this repository |
| `repository_archived` | 409 | the repository is archived |
| `repository_changed` | 409 | the repository the replayed request created no longer exists |
| `request_failed` | 400 | the request failed |
| `reset_link_invalid` | 422 | that password reset link is expired, already used, or not a link this forge issued |
| `runner_policy_denied` | 422 | the runner policy denies this trust tier on the chosen runner |
| `sandbox_unavailable` | 424 | a verified Firecracker runner is unavailable |
| `serialization_failed` | 500 | the response could not be serialized |
| `service_unavailable` | 503 | the service is temporarily unavailable |
| `session_already_published` | 409 | the session was already published |
| `session_invalid_request` | 422 | the session request body failed validation |
| `session_not_recorded` | 503 | the agent session could not be recorded, so it was not launched |
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
| `task_not_granted` | 403 | the bot grant does not allow this task |
| `todo_action_forbidden` | 403 | the caller may not run that action on the todo |
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
| `tool_unavailable` | 424 | the MCP tool has no execution adapter installed on this server |
| `trace_collect_failed` | 500 | the work trace could not be collected |
| `trace_events_unreadable` | 500 | the activity log could not be read for the work trace |
| `trace_invalid_query` | 422 | the trace names neither one todo nor one pull request |
| `trace_pull_not_found` | 404 | this forge does not host that pull request |
| `trace_todo_not_found` | 404 | no family queue holds that todo |
| `unauthorized` | 401 | the request needs a login or token |
| `unsupported_media_type` | 415 | the request body needs Content-Type: application/json |
| `usage_invalid_query` | 422 | the token usage query failed validation |
| `usage_store_failed` | 500 | the token usage sample store failed |
| `verification_link_invalid` | 422 | that email verification link is expired, already used, or not a link this forge issued |
| `waitlist_full` | 503 | the waitlist is not accepting new addresses |
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
| `yield_read_failed` | 500 | the todo queues or the cost ledger behind the yield report could not be read |

## CLI Exit Codes

The `jeryu` CLI turns a failure into a process exit code, so a shell script can
route without parsing output. The codes are stable:

| exit | meaning | HTTP statuses that map here |
| --- | --- | --- |
| 0 | the command succeeded | 2xx |
| 1 | the CLI itself could not start (argument parsing, `serve`) | — |
| 2 | the entity was not found | 404 |
| 3 | the request conflicts with current state | 409 |
| 4 | the request is not acceptable as sent | 400, 422, any other 4xx |
| 5 | the capability is not wired to a live engine | — |
| 6 | not authenticated, or not allowed | 401, 403 |
| 7 | the service failed | 5xx |
| 8 | rendered output could not be written or flushed | — |

With `--json`, a failure is one envelope on stdout carrying the fields above
plus `exit_code`. When the failure came from a live API, it also carries
`http_status` and `body` — the API's answer verbatim, including an error body
that is not JSON (kept as a string) — and the API's own `code`, `reason`,
`common_fixes`, `repair_hint` and `docs_url` win over the CLI's defaults.

Exit 8 covers a closed stdout (`jeryu ... | head`) and a full disk. The CLI
never reports success for output it could not deliver.

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

## Idempotent Writes

A `POST` under `/api/v1/` or `/repos/` (and `/api/v3/repos/`) may carry an
`Idempotency-Key` header (1-255 visible ASCII characters). The first request
runs; a repeat with the same key, credentials, method, path and body within 24
hours answers the kept reply again with `Idempotent-Replayed: true` and does not
run the write twice. A repeat while the first still runs answers
`idempotency_key_in_flight` (retry later); the same key on a different request
answers `idempotency_key_reused` (use a fresh key). 5xx answers are not kept, so
a retry after a server error runs the write. A write whose answer holds a
once-shown secret (bot enrollment, key rotation, agent tokens, token exchange
and refresh, personal access token creation, login) keeps only a placeholder: a
repeat answers 409 `idempotent_secret_withheld` and still does not run the
write twice, and the secret never reaches the store. A kept answer also carries
no `Set-Cookie`, so a session published by a write is never left at rest. A successful session-cookie write, including signup and password change, replays as 409 `idempotent_secret_withheld` instead of a success without the session. The
keys are kept in `<data_dir>/shift.sqlite`, so a retry that arrives after a restart or a deploy
still replays the first answer; when that store cannot be read the write is
refused with `idempotency_store_failed` rather than run a second time. `PUT`,
`PATCH` and `DELETE` set a named resource and are idempotent without a key.

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
the host entry with `jeryu gh-setup --host <the-jeryu-url-you-reached>
--token-file <your-token-file>`, then use `/api/v1/capabilities`, the Jeryu REST
routes, or the `jeryu.*` MCP tools for the original PR, CI, issue, or repository
task.

If `gh` reports a stale or invalid token for an existing Jeryu host entry,
rerun `jeryu gh-setup --host <the-same-host> --token-file
<your-token-file>`. GitHub.com auth and local Jeryu host auth are
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

## v3 List Ordering And Filtering

The GitHub-compatible lists order and filter themselves from the query string:

| Route | Query |
| --- | --- |
| `GET /api/v3/repos/{owner}/{repo}/pulls` | `state=open\|closed\|all`, `sort=created\|updated\|popularity\|long-running`, `direction=asc\|desc`, `head=owner:branch`, `base=branch` |
| `GET /api/v3/repos/{owner}/{repo}/issues` | `state=open\|closed\|all`, `sort=created\|updated\|comments`, `direction=asc\|desc` |
| `GET /api/v3/repos/{owner}/{repo}/commits` | `sha=`, `direction=asc\|desc` (and `sort=created`, its only orderable field) |

The default order is `created`/`desc` — newest first — so the pull request
opened last leads the first page however many there are. `sort=updated` and
`sort=popularity` (comment count) default to `asc`, as on GitHub;
`sort=long-running` orders by how long a pull request has been open and
defaults to `desc`, longest-open first. `state` defaults to `open`. `head`
accepts GitHub's `owner:branch` form and a bare branch name, which is how an
agent asks "does my pull request already exist?" and gets that one pull
request rather than the first page of everything.

A value none of these accept is refused with GitHub's `422 Validation Failed`
naming the field and listing the accepted values, never ignored:

```json
{
  "message": "Validation Failed",
  "errors": [
    {"resource": "PullRequest", "field": "sort", "code": "invalid", "value": "bogus",
     "accepted": ["created", "updated", "popularity", "long-running"]}
  ]
}
```

Pagination `Link` relations are absolute `/api/v3` URLs and carry the caller's
own query, so following `next` stays in the order and filter the first page was
read with. See `docs/pagination.md`.

## Invalid Page Parameter

Both edges follow one paging rule, written once in `docs/pagination.md`: an
out-of-range paging parameter is a 422, never a clamp.

On `/api/v1`, `limit` (alias `per_page`) must be 1 to 500 and defaults to 100
and `page` is 1-based; a value out of range answers `invalid_page_parameter`
(422), and `GET /api/v1/events` answers its own `events_invalid_query`. Each
response carries a `page` object with the applied `limit`, `page`, the
pre-paging `total` and `has_more` (`/api/v1/events`, a cursor walk, answers
`limit`, `has_more` and `next_cursor`).

On `/api/v3`, `per_page` must be 1 to 100 (GitHub's ceiling) and defaults to
30; out of range answers GitHub's 422 `Validation Failed` with a `Pagination`
error entry, and the page relations ride in the `Link` header.

## Invalid Query

A filter that can never match is a mistake in the request, not an empty
collection: answering `200` with `[]` reads as "there is nothing to do". Every
list route below reads its query through `StrictQuery`
(`crates/jeryu-api/src/web/strict_query.rs`), which refuses

- a key the route does not read (`?statuss=open`), naming every key it does, and
- a value outside a closed set (`?status=bogus`), naming every value it accepts,

with `invalid_query` (422). `common_fixes` lists the keys the route reads, so a
client repairs the request from the refusal alone.

| Route | Closed sets |
|---|---|
| `GET /api/v1/shift/todos` | `status` (`open`, `claimed`, `done`, `blocked`, `handoff`, `parked`, `closed`), `mode` (`now`, `night`) |
| `GET /api/v1/shift/workers` | `family` (a hosted family; see `docs/family-key.md`) |
| `GET /api/v1/merge-queue` | `state` (`building`, `landed`, `failed`, `dequeued`, or `all`; default `building`) |
| `GET /api/v1/attention` | `severity` (`critical`, `action`, `watch`), `kind` (the inbox kinds, `KINDS` in `attention.rs`), `family` |
| `GET /api/v1/events` | keys only; it keeps its own `events_invalid_query` code and validates `kind` and `limit` itself |
| `GET /api/v1/events/histogram` | `bucket` (`5m`, `15m`, `1h`, `1d`); it keeps the log's `events_invalid_query` code and validates `hours` and `kind` itself |

A `family` that names no hosted family answers `family_unknown` (422) for the
same reason, on every route that takes one.

## Agent-run attach frames

`GET /api/v1/agent-runs/{id}/attach` is a WebSocket. Once it is open, problems
arrive as `{"type": "error", "code": ..., "request_id": ...}` frames rather
than HTTP errors:

| Code | Meaning |
| --- | --- |
| `attach_message_invalid` | The client frame was not a recognized JSON control frame |
| `attach_viewer_limit` | The run already has the maximum number of attached clients |
| `control_forbidden` | Only an admin may drive a host-PTY agent run |
| `control_not_held` | This socket does not hold the control lease; attach with `control=true` or send `take_control` |
| `control_rejected` | The driver did not accept the command, or another client took control first |
| `agent_run_finished` | The run has already finished |

`control_lost` (another client took control) and `ended` (the run finished
and its output drained) are status frames, not errors.
