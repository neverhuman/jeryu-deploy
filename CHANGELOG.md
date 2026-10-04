# Changelog

## Unreleased

- Releasing a todo a worker is still running needs `force`. `POST
  /api/v1/shift/todos/:family/:id/action` with `{"action": "release"}` answers
  `409 claim_live` while the todo's claim lease is live, because the released
  todo would be claimed by a second worker while the first is still working on
  it; `{"action": "release", "force": true}` takes it back anyway. A release
  now replaces the note it finds — with the one the body carries, or with
  nothing — so a released todo no longer reads as the reason it stopped.

- Kept answers to writes that carried an `Idempotency-Key` live in
  `<data_dir>/shift.sqlite` (`db/migrations/0007_idempotency_keys.sql`) rather
  than in memory, so a retry that arrives after a restart or a deploy replays
  the first answer instead of filing the work twice. A reservation whose
  request never answered is freed when the store is opened, and a write whose
  key cannot be read answers `500 idempotency_store_failed` rather than run a
  second time.

- A pull request's head can be gated again: `POST
  /api/v1/repos/:id/pulls/:n/regate` records the ask against its current head,
  and `GET /api/v1/gate-regate?state=pending` is what the gate runner reads each
  tick, so a head whose gate failed for a reason outside its own sources no
  longer needs a push or a shell on the gate host. The attention inbox carries
  the route as the `action.api` of a `pr_checks_failing` item whose failing check
  the base requires (`docs/repo-automation.md`).

- The split family's membership and release identity are read from the one
  authority manifest `jeryu-release-ops` publishes; this repository no longer
  ships a `repos.manifest.toml` of its own. `jeryu-split` finds that file
  through `JERYU_FAMILY_MANIFEST` or a `jeryu-release-ops` checkout beside this
  one (`jeryu-split manifest-path` prints which), and reads its shape:
  `required_repos` names every member, the control plane sits under
  `[control_plane]`, and every other member is one `[[repo]]` row. The
  `source-coverage` command is gone with the monorepo assignment fields it
  read, and `product-pipeline` now runs the manifest and lock checks. Hosted
  tool-finder discovery reads the same member list, so the family's control
  plane is no longer left out of a scan.

- `jeryu autonomy init` writes `.jeryu/ci.toml` in the shared schema "2":
  `schema_version = "2"`, `provider = "jeryu"` and one `[[lane]]` named
  `required` whose `command` is exactly what the forge gate runs — `just
  required` when the repository's justfile has a `required` recipe, else `bash
  ops/ci/pr-ci.sh` when that script exists, else `just required`. A new
  `--lane-command` flag overrides the detection. The retired
  `github_actions_required` key is no longer emitted, and the optional `runs`
  list is omitted.
- The jankurai audit runner shows on `/runners`, with the tools it evaluates
  with. A runner heartbeat may now carry `tools: [{name, version?, sha256?}]`
  (at most 32, names unique, `sha256` 64 lowercase hex; a refusal names the
  entry, `tools[2].sha256: …`), echoed per node in
  `GET /api/v1/control-plane/runners`. A new `jankurai-audit` label marks the
  audit runner, with its own conclusions (`scored`, `tool-failed`, `refused`,
  `failed`); it holds no gate slot and emits no pipeline events. Every node now
  carries `kind` (`gate`, `reviewer`, `automation`, `deployer`,
  `jankurai-audit`, `workcell`), and deploy timers no longer count toward
  `onlineRunners`. `ops/ci/jankurai-audit-runner.sh` beats on every run as
  `<host>/jankurai-audit` (`current` while auditing, the last result kept
  between runs, `code` from `JERYU_AUDIT_RUNNER_REPO` and the install's
  `VERSION`, `tools` measured from the governed binary), best-effort, retrying a
  `422` once without `code`/`tools`.
- A repository page can say what runs on it.
  `GET /api/v1/repos/:id/automation` answers with the repository's checks (and
  every required context that has never reported), the reviewer and merge
  identities with the grant each needs — a merger without its write grant is
  flagged, because its merges answer 403 — the gate runners and deployers that
  touched the repository, its push mirrors with the sha each holds and how far
  behind the forge it is, and its grants for a caller who may administer it.
  Host deploy timers report through the existing runner heartbeat with the new
  `deploy` label and a mandatory `last.target`, so a timer can say it deployed a
  sha to a target and whether that worked. See `docs/repo-automation.md`.
- Jankurai scoring runs on the gate runners, not on the forge host. A push now
  records at most one audit job per head (branch, head sha, base sha) in the
  forge's audit queue and publishes `jankurai/proof` as pending; it runs no
  clone, no checkout and no auditor. Runners claim jobs
  (`POST /api/v1/jankurai-audits/claim`, `GET /api/v1/jankurai-audits`) with
  `ops/ci/jankurai-audit-runner.sh` and submit the report through
  `POST /api/v1/repos/:id/jankurai-scores`, which accepts it only from a runner
  identity allowed to score (`JERYU_JANKURAI_SCORERS`), only
  against an open job for exactly that branch, head and base, and only from the
  governed auditor's version and sha256 — and derives the verdict from the
  report itself. Only pull request heads and the protected `main` are audited;
  `import/`, `preserve/`, `archive/`, `archives/`, `bot/` and `auto/` branches
  create no job; a head with no merge-base gets a neutral `no base branch yet`
  instead of a whole-repository audit; and with no runner the proof stays
  pending rather than going green. `<repo>/required` submits its own audit of
  the head (`ops/ci/pr-ci.sh`), so a head is audited once. Flow:
  `docs/governed-jankurai.md`.
- `jankurai/proof` is a gate, not a report: `ops/ci/jankurai-gate.sh` gives every
  repository the hosted verdict locally before a PR is opened, and for a
  repository under the rollout (`JERYU_AUDIT_GATE_REPOS`,
  `agent/jankurai-gate.toml`) an approval and a merge are refused while the proof
  on the exact head fails, is missing, or failed to score. See
  `docs/jankurai-gate.md`.
- `jankurai/proof` says why it failed and links a report a reader can open. A
  `tool-failed` check carries the audit's own error in its title and summary
  (`the auditor exited 3: no usable base ref: refs/heads/main does not exist`)
  and stores it with the score as `host_error`, so the Quality gate page of the
  head reads the same words; a below-floor check lists its top findings with
  `path:line` in `output.text`. The check's `details_url` is the configured
  public origin (`JERYU_PRODUCTION_ORIGIN`) rather than the pushing client's
  `Host`, and a loopback or tunnel address is dropped instead of published, so
  one head gets one proof instead of one per address it was pushed through: a
  re-audit that reaches the same verdict posts nothing. A PR check row now
  carries `web_url_label` ("View report") and `details_text` so the Checks panel
  can show the link and the findings behind a score.

- A `gate.finished` or `review.finished` event that went wrong always says why.
  `POST /api/v1/runners/heartbeat` accepts `last.reason`, and a runner that
  sends none still gets a reason derived from its conclusion, so "Review failed
  ... needs you" no longer appears with nothing to act on. Repeats collapse: the
  same verdict on the same head, beaten again, is one event.
- A direct merge no longer lands an ungated replay. A base that requires linear
  history had the forge replay a diverged pull request onto the live tip and
  fast-forward to it: the gate had run on the PR head, so main's new sha carried
  no `<repo>/required` of its own, auto-pin sat at "gate is pending; waiting"
  and the crate commits to be tagged had no gate evidence (jeryu-web #63/#64 and
  jeryu-core #17 on 2026-09-28, worked around with empty helper PRs). The merge
  now refuses such a replay with `409` naming the missing context, and the pull
  request lands through the merge queue, which gates the replay at its exact sha
  before the base moves. A base that declares no required contexts is unchanged.

- Redeploying the release production already runs is a no-op, not an alert.
  `deploy-release.sh` reads the live release before it records anything: if it
  is the requested one it says "already live" and exits 0, recording no
  deployment (`--json` answers `already_live: true`); the staged `switch.sh`
  does the same when run on its own, without stopping the service. The
  `deploy_failed` attention item is `watch` rather than `critical` when the
  failed attempt left production as it was — the live deployment is a
  successful one of the release the attempt tried to deploy. Both deployment
  statuses now carry a `log_url` pointing at the `deploy.status` events, which
  carry the switch's log tail and its path on the release host
  (`JERYU_RELEASE_LOG_URL` overrides it).

- Dependency majors: workspace `sha2` 0.11 with `hmac` 0.13, `toml` 1, `thiserror` 2,
  `reqwest` 0.13 (jeryu-cli and jeryu-api's dev-dependency) and `base64` 0.23 in
  jeryu-api. `sha2` 0.11 no longer formats a digest with `{:x}`, so digests are
  spelled `hex::encode(...)`. `reqwest` 0.13 brings aws-lc-rs, so the `deny.toml`
  licence allow-list gains `ISC`, `MIT-0` and `CDLA-Permissive-2.0`.

- The web console's Quality gate pages have their API: `GET /api/v1/quality-gate/overview?days=7|30`,
  `GET /api/v1/quality-gate/rules/:rule?days=`, `GET /api/v1/quality-gate/heads/:owner/:name/:sha` and
  admin-only `POST /api/v1/quality-gate/findings/:id/dispute` serve the stored jankurai scores and disputes
  in the shape `/quality-gate` reads (a finding id is `<score_id>:<index>`). Until now the page's nav item
  pointed at a 404.

- `/runners` can show the release timers. `POST /api/v1/runners/heartbeat` accepts the `automation` label (`last.conclusion` of `opened`, `staged`, `waiting` or `failed`), an optional `pr` for every label (auto-stage stages a commit, not a pull request) and an optional `intervalSeconds` (30 to 86400): a runner is then offline after `max(180, 3 * intervalSeconds)`, returned per runner as `offlineAfterSeconds`. Forge admins may report as well as the `JERYU_RUNNER_REPORTERS` logins. Automation beats hold no gate slot, never count towards `gate_runner_down` and emit no `gate.*`/`review.*` events. `auto-pin.sh` and `auto-stage.sh` beat once per tick (`JERYU_AUTO_PIN_BEAT=0` / `JERYU_AUTO_STAGE_BEAT=0` turn it off). Contract: `docs/pipeline-events.md#runner-heartbeats`.

- Repo graph v2 (`jeryu.repo_graph/v2`): `depends_on` edges between
  repositories, derived from every repository's Cargo manifests at its indexed
  default-branch ref. Each edge carries the tag it pins, the newest tag of that
  tag's series and whether the pin is that newest tag, so a stale pin is
  visible on the edge. The edges are opt-in behind
  `GET /api/v1/control-plane/repo-graph?include=depends_on` (the Intelligence
  page's graph is unchanged) and the manifest reads are cached for a minute
  rather than walking every repository per request. Repo nodes now say
  `releaseMember`, read from the split family manifest.

- Quality-gate visibility before the `jankurai/proof` gate is made required:
  `GET /api/v1/jankurai/overview?days=7|30&repo=` (pass/fail counts and rate in
  daily buckets, per repo and overall, failures by cap or hard rule with the
  repos each affects, score distribution, and the failing heads that merged
  anyway), `GET /api/v1/jankurai/rules/:rule_id` (the heads one rule or cap
  flagged, with repo, sha, pull request, score, floor and caps),
  `GET /api/v1/jankurai/scores/:score_id` (score, raw score, floor, applied
  caps, hard findings and every finding with rule id, path, line and evidence,
  parsed from the report each score already stores), and admin-only
  `POST /api/v1/jankurai/disputes` with a `GET` listing, persisted in
  `<data_dir>/shift.sqlite` by `db/migrations/0003_jankurai_disputes.sql` and
  counted per rule in the overview.

- Attention inbox: `mirror_failing`, one item for the whole forge when GitHub mirror pushes fail (all eight configured mirrors on the hosted forge had never succeeded, and nothing said so).

- API answers an agent can act on: an unmatched path under any `/api/` version is a JSON 404 (v3 paths still fell through to the web app's HTML with a 200); `GET /api/v1/shift/todos` and `/shift/shifts` answer `404 shift_family_not_found` for a family nobody hosts, not an empty list; `GET /api/v1/events?kind=` refuses a filter that is neither a kind nor a dotted prefix with `422 events_invalid_query`.

- Attention inbox accuracy, from the first live walk: reasons open with the
  specific fact (a blocked todo's note verbatim), `shift_without_pr` needs
  `unmerged_todos` so a branch replaced by a rebased one is quiet, and a failing
  check the base branch does not require is a `watch` item, not an `action`.
  `GET /api/v1/shift/families` names each repo's hosting `owner`. The source
  browser answers 404, not 500, for a path that is not at the ref. A merged or
  closed pull request's passport carries one plain blocker.

- Add `GET /api/v1/pins` (what each deploy repo pins, how far behind it is, what
  a bump would ship, whether a bump is open) and the `pin_behind` attention
  kind; add `scripts/release/auto-pin.sh` with its timer and installer, which
  opens the jeryu-web pin bump pull request once jeryu-web main is green and
  never merges. Contract: `docs/pipeline-events.md#pins`.

- Add the pipeline event log (`POST`/`GET /api/v1/events`, the admin-only
  `pipeline` WebSocket scope, server-side events for todos, worker stages, pull
  requests, the merge queue, runners and deployments) and the attention inbox
  (`GET /api/v1/attention`); derive `merged`, `released`, `pr` and `cost_usd` on
  `GET /api/v1/shift/todos`; have `auto-stage.sh` report staged releases; and
  answer unknown `/api/v1/` paths with a typed JSON 404 instead of the web
  app's HTML. Contract: `docs/pipeline-events.md`.

- Decompose the agent-run web control surface into handler, bounded-store,
  frozen-diff export, and focused TTY regression modules; preserve the existing
  route and wire behavior behind a new narrow `just agent-runs` proof command.
- Split the remaining oversized API and split-tool sources at their existing
  catalog, bootstrap, session-runtime, Git-source, pull-posture, installed-audit,
  and test boundaries without changing their public paths or wire contracts.
- Propagate a bounded `x-request-id` across both HTTP and MCP responses, replacing
  hostile or oversized caller values and covering the boundary with focused tests.
- Make `tools/security-lane.sh` the canonical executable security authority,
  keep the historical ops path as a compatibility delegate, and require full
  Cargo/npm dependency audits from the comprehensive and PR validation lanes.
- Reserve external check-run, commit-status, and Jankurai score publication for
  global-admin maintenance while native runner results remain server-published.
- Require repository-admin authority for branch-protection changes instead of
  allowing any repository writer to weaken the evidence gate.
- Recompute push-time Jankurai evidence before adopting stored state so an
  interrupted or tool-failed audit can recover on the same commit; reject
  incomplete/nonzero tool output, audit a first main ref against an empty tree,
  and prevent candidate policy from lowering the host score floor.
- Include the security lane in the canonical required-check entrypoint and keep
  its workflow declaration aligned with the commands the lane actually runs.
- Repair the standalone full and affected CI paths so they use repository-owned
  proof, workflow, release-receipt, score, and security gates instead of absent
  monorepo packages, and validate owner/test coverage from the tracked tree.
- Make all ten phase gates exercise only real owned integrations or resolvable
  immutable dependencies, validate the vendored web bundle without npm source,
  and stop the PR gate from silently restoring a changed `Cargo.lock`.
- Rebind the API coverage floor once from unreproducible pre-split `0.8411` to
  `0.8044`: exact hosted protected main measured `0.7981`, while this candidate
  improves it to `0.8044`; all later baseline updates remain upward-only.
- Bound coverage-test concurrency independently from compiler concurrency so
  process-heavy identity and live-route tests do not fail under host pressure.
- Apply the same eight-process default to aggregate CI tests and stabilize the
  fsynced executable test fixture with a bounded Linux `ETXTBSY` readiness
  check without retrying or weakening the production identity verifier.
- Keep deleted files in the protected-base proof plan while passing only the
  exact extant changed-path subset to Jankurai 1.6.11 proofbind, and assert both
  scopes so stale evidence removal cannot be mistaken for a missing input.
- Replace duplicate proof scripts and their swallowed failures, candidate
  self-baseline, and synthesized UX/migration/vibe/coverage outputs with one
  strict standalone lane backed by a provenance-bound hosted-main baseline;
  make source-security receipts name only commands that actually ran.
- Supply Jankurai's canonical `tools/security-lane.sh` entrypoint as a governed
  delegate to the real standalone security implementation, with explicit owner
  and test-map coverage.
- Add locked, package-scoped API check/test commands and an explicit sccache
  status probe for fast deterministic developer feedback, and make every
  canonical Rust CI/build entrypoint refuse dependency-lock drift.
- Patch the locked `anyhow` and `h2` advisories, keep Cargo Deny default-deny,
  and route every historical Cargo Git identity through exact
  `git.neverhuman.org` mappings and immutable hosted support refs with
  fresh-cache and hostile regression proof.
- Require CI verification and publication to use the one exact hosted origin
  with no alternate push URL, while allowing the accepted predecessor runtime
  to stay live during candidate tests that select freshly built binaries.

## jeryu-deploy-v5.0.0-split.4

- Bind pull-request reviews and merge protection to the exact current head via
  immutable Jeryu Core split.5, preserving historical reviews as stale audit
  evidence and applying latest-review-per-reviewer precedence.
- Bind hosted MCP and agent entry points to authenticated principals rather
  than request-supplied actor names.
- Rotate authenticated sessions after password changes so the old auth epoch is
  revoked while the caller receives a fresh cookie and CSRF token.
- Isolate live Git LFS transport tests from user and system Git configuration so
  global filter hooks cannot mutate or falsely fail the disposable repository.

## jeryu-deploy-v5.0.0-split.3

- Decode bounded gzip/x-gzip Git smart-HTTP pack RPC requests before invoking
  Git, reject malformed or stacked encodings, and preserve protocol-v2 headers.
- Govern every active Jankurai consumer on the local-authority 1.6.11 split.2 tag and
  exact binary digest and installation receipt, with physical-file,
  wrong-authority, hostile-substitution, and PATH-neutralization tests plus
  root-broker release-custody controls; retain the 1.6.10 score baseline only as
  non-authoritative history.

## jeryu-deploy-v5.0.0-split.0 - 2026-06-11
- MAJOR: first standalone split-family release; the legacy monorepo
  is deprecated and its drift fully reconciled.

## jeryu-deploy-v4.0.0-split.0

- Initial split-family baseline for `jeryu-deploy`.
