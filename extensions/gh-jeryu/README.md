# gh jeryu

A read-only gh extension for the native Jeryu PR workflow on Linux/macOS. Requires Python 3.10+
(standard library only) and gh 2.40 or newer; tested with gh 2.45.0.
It uses `gh api` authentication for the selected host. Configure that host with
`jeryu gh-setup` as described in [the setup guide](../../docs/errors.md#reproducible-gh-api-setup); no separate token store is
created. The native server must return `workflow.schema = pr_workflow/v1`.
An older server without this block returns `unsupported_response`; it never
infers readiness from a partial response.

From this checkout, install the local extension explicitly:

```sh
cd extensions/gh-jeryu
gh extension install .
gh jeryu pr status 42 --repo acme/demo --host https://git.example.org
gh jeryu pr status 42 --repo acme/demo --host https://git.example.org --json
gh jeryu pr wait 42 --repo acme/demo --host https://git.example.org --target ready --timeout 300
gh jeryu pr wait 42 --repo acme/demo --host https://git.example.org --target merged --timeout 600 --json
```

The repository owns the extension; installation is an operator action, separate
from releases. To run without installing, use `./extensions/gh-jeryu/gh-jeryu`.
`--host` is an origin, without `/api/v1`; HTTPS is required except on loopback
for disposable forges. `--repo`, the PR number and the wait target are explicit.

Human output includes the head, checks, review, blockers and next action. JSON
contains `result`, `selected_head_sha` and the unmodified aggregate `workflow`
block. No next action is executed. Status exits 0 after a valid observation,
including a blocked PR. Wait binds to the first observed head, or to `--head`
when a full expected SHA is supplied. Any different observed/current head
exits immediately, even if the new head is ready or merged. A ready target
requires a ready observation; an already merged PR is terminal for that target.
Review-needed and pending checks continue waiting. Draft, blocked, failed checks
and closed PRs stop immediately.

Wait requires `--timeout` in seconds (greater than 0, at most 86400), measured
with a monotonic clock and including request time. Requests are capped at 30
seconds or the remaining budget. Ctrl-C or SIGTERM cancels the child request
and wait. Exit results:

| Exit | Result |
| --- | --- |
| 0 | status observation / wait target reached (`success`) |
| 1 | request, missing gh, or unsupported response error |
| 2 | blocked, failed checks, draft, closed, or merged before a ready observation; also invalid arguments |
| 3 | timeout |
| 4 | authentication/authorization error, including HTTP 401/403 |
| 5 | head_changed; start a new wait to select another head |
| 130 | cancelled |

Only the aggregate native `GET /api/v1/repos/{owner}/{repo}/pulls/{number}`
is polled. The current repository WebSocket notifications cover runner changes,
not all PR workflow transitions; pipeline event reads require admin access.
Neither supplies a usable PR notification stream for this `gh api` client.
Polling sleeps 1, 2, 4, 8, then 15 seconds between attempts, bounded by the
remaining timeout. Network timeouts, transport errors, HTTP 408/429 and 5xx
retry within that same budget; all other errors stop. gh diagnostics are not
printed, avoiding credential-helper output in results.

The required API test suite runs `gh_extension_waits_on_native_forge_and_keeps_the_selected_head`
against a disposable in-memory forge over loopback with the real gh binary and
isolated HOME/gh configuration. It covers pending-to-ready, ready-to-merged,
failed checks, head movement and timeout. Run it with:

```sh
cargo test --offline -p jeryu-api --features web gh_extension
```
