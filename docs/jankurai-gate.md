# The jankurai gate: local before the PR, required before approval

Decision, 2026-09-29 (it supersedes the 2026-09-19 "shadow mode until the
Quality gate view has a week of data" plan): the jankurai audit is a gate, not a
report. It runs locally once **before** a pull request is opened, and
`jankurai/proof` must pass on the exact head before that pull request can be
approved or merged.

## One command, before the PR

```sh
ops/ci/jankurai-gate.sh              # HEAD against the merge-base with origin/main
ops/ci/jankurai-gate.sh --base-ref <rev>
just jankurai-gate
```

It runs the same governed auditor, over the same diff, with the same floor and
the same strictness as the hosted `jankurai/proof` (score below the effective
floor, any applied cap, any hard finding, or an audit that produced no score all
fail), and it prints the words the hosted check-run will carry. `cargo test -p
jeryu-api --features web jankurai_gate_script` renders a set of reports through
both the script and `ci_bridge::jankurai::jankurai_proof_output` and requires
them to agree, so the local verdict and the hosted one cannot disagree about one
sha. Every repository of the family carries the same script at the same path,
and `ops/ci/pr-ci.sh` runs it.

The gate refuses a pull request only where the rollout is on; elsewhere it
prints the same verdict and exits 0.

## The gate on the forge

For a repository under the rollout, `jankurai/proof` becomes a required context
on every pull request, which means:

- `POST /api/v1/repos/:id/pulls/:number/approve` refuses with
  `approval_blocked_jankurai_proof` (409) while the proof on the exact head
  fails, has not run, or has not finished. The refusal carries the proof's own
  verdict text and a link to the report, so the pull request shows why.
  pr-redteam (the reviewing agent) holds such a head with the same reason before
  it spends any review budget.
- The merge passport is blocked, so `POST .../merge` refuses, and the forge's own
  branch-protection evaluation lists `JankuraiProofRequired`.
- An actual scorer failure is not a pass: the push records a `tool-failed` score
  and publishes a failing proof whose title is the reason, and that reason is
  what blocks.

## A blocker that is red on purpose, and who can clear it

A change that lowers a floor fails the very context the floor protects. That is
the gate working, and it leaves the pull request blocked with nothing an owner
can press: root/jankurai-deploy#8 (2026-10-14) showed an administrator
"Blocked: 3 things need doing" and no Merge button, with the only known routes
being to turn branch protection off and back on, or to push an empty commit so
a held review ran again.

`GET /api/v1/repos/:id/pulls/:number/owner-paths` answers the other half of
that page: for the account asking, which of the passport's blockers a
repository administrator can clear, and the step that clears each one. Three
things hold it honest:

- every blocker of the passport appears, in the passport's own order, whether
  or not the reader can clear it. The route explains the gate; it never
  shortens it;
- `viewer_may_clear_as_owner` is true only where the base branch's protection
  leaves administrators out (`enforce_admins: false`) **and** the caller
  administers this repository. Both are read per request;
- every step is one the forge serves today — a review to post, a thread to
  resolve, a required context an administrator owns in repository settings.
  Turning protection off is not a step and is not offered.

For the hold that pull request actually tripped on, the step is a recorded
decision: approve the exact head with a comment that states it ("Owner
decision: …"). pr-redteam re-reads the approvals immediately before it
publishes, so a decision that lands while a review is running still answers the
findings that asked for one — and those findings stay in the published review.
That behaviour lives in the reviewer, not in this repository: jeryu-ci-runner
`ops/pr-redteam/pr-redteam` ("owner decisions") and its `test/owner-decision.sh`,
described in `ops/pr-redteam/README.md`.

## A repository that does not use the audit

Decision, 2026-10-07: a repository that holds no code — a private markdown
wiki, a notes repository — turns the audit off in its **own** `.jeryu/ci.toml`,
the schema-2 file jankurai already reads for the CI-present credit. There is no
site setting for it: the repository states what it is.

```toml
schema_version = "2"
provider = "jeryu"

[[lane]]
name = "required"
command = "just required"

[jankurai]
audit = false   # default true: queue an audit for main and pull request heads
```

`jeryu autonomy init` writes the key at its default with that comment. With
`audit = false` the push path records no audit ticket and publishes no
`jankurai/proof` at all, so no runner time is spent and no advisory check
appears on the repository's pull requests.

Two rules keep it from being a way out of a gate:

- **The base decides, not the head.** For a pull request head the key is read
  from the base branch's `.jeryu/ci.toml`, so a pull request that adds
  `audit = false` to itself opts out nothing. For a push to `main` it is read
  from the pushed commit.
- **A required proof wins.** If the base branch's protection rule requires
  `jankurai/proof` (or the site enforces the gate for the repository — see
  below), the head is audited whatever the key says, and the pending proof
  carries a note that the opt-out was ignored and why.

A declaration that cannot be read — not valid TOML, `audit` set to something
that is not a boolean, or a file larger than 64 KiB — turns nothing off: the
head is audited as it was before the switch existed and the proof says which
file could not be read and why.

Where the gate is not on and the repository does use the audit, `jankurai/proof`
stays an advisory row on the checks tab. A below-floor score there is a report
nothing waits on, so the row reads **neutral**, not failure red.

## Rollout, per repository

Two switches, and they belong on together — a local refusal with a hosted
approval, or the reverse, is worse than either state:

| Where | Setting |
| --- | --- |
| Local (`ops/ci/jankurai-gate.sh`, `ops/ci/pr-ci.sh`) | `agent/jankurai-gate.toml`, `enabled = true`; `JERYU_JANKURAI_GATE=1/0` overrides for one run |
| Hosted (approval, merge passport) | `JERYU_AUDIT_GATE_REPOS="jeryu/jeryu-deploy jeryu/jeryu-ci-runner acme/*"` on the API unit — `owner/name` entries, comma or whitespace separated, `owner/*` for a whole owner |

`JERYU_AUDIT_ENFORCE_MERGE=1` still turns the gate on family-wide, and the
intrinsic merge gate in jeryu-core reads that flag; the per-repo setting exists
so the rollout does not have to be all-or-nothing.

Why per repository: a repository whose main scores below the floor would have
every pull request blocked by a family-wide flip until its caps were fixed.

**Report before enabling.** A repository goes under the gate only after its main
passes. On a host with the governed auditor installed, for each repository:

```sh
git -C <repo> fetch origin main
git -C <repo> checkout --detach origin/main
<repo>/ops/ci/jankurai-gate.sh --base-ref "$(git -C <repo> rev-parse origin/main~1)"
```

### The report of 2026-09-30

Measured with the pinned auditor (`jankurai 1.6.11`) through
`ops/ci/jankurai-gate.sh`, against the merge-base with `origin/main`:

| repository | score | floor | caps | hard findings | verdict |
| --- | --- | --- | --- | --- | --- |
| `jeryu/jeryu-ci-runner` | 93 | 85 | none | 0 | pass — gate on |
| `jeryu/jeryu-deploy` | 86 | 85 | none | 0 | pass — gate on |

The two repositories that pass have `enabled = true` in
`agent/jankurai-gate.toml`. The family's other repositories have not been
measured here (this change was made in a two-repository clone) and stay off until
they are: run the loop above on the host, where every repository has a working
tree, and extend the table with the result before adding a name to
`JERYU_AUDIT_GATE_REPOS`.

The exit status is the answer, and the printed verdict is the reason. Record the
table of repository, sha, score, floor and caps with the rollout change that
turns the gate on; a repository whose main does not pass stays off until its caps
are cleared.
