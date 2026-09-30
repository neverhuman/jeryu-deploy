# The jankurai gate: local before the PR, required before approval

Owner decision, 2026-09-29 (it supersedes the 2026-09-19 "shadow mode until the
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

## Rollout, per repository

Two switches, and they belong on together — a local refusal with a hosted
approval, or the reverse, is worse than either state:

| Where | Setting |
| --- | --- |
| Local (`ops/ci/jankurai-gate.sh`, `ops/ci/pr-ci.sh`) | `agent/jankurai-gate.toml`, `enabled = true`; `JERYU_JANKURAI_GATE=1/0` overrides for one run |
| Hosted (approval, merge passport) | `JERYU_AUDIT_GATE_REPOS="jeryu/jeryu-deploy jeryu/jeryu-ci-runner veox-ai/*"` on the API unit — `owner/name` entries, comma or whitespace separated, `owner/*` for a whole owner |

`JERYU_AUDIT_ENFORCE_MERGE=1` still turns the gate on family-wide, and the
intrinsic merge gate in jeryu-core reads that flag; the per-repo setting exists
so the rollout does not have to be all-or-nothing.

Why per repository: on 2026-09-29 `veox-ai/veox-telemetry` main (`0ada588`)
scored 47 against a floor of 85. A family-wide flip would have blocked every
pull request that repository has until its caps were fixed.

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
| `veox-ai/veox-telemetry` | 47 | 85 | see its report | — | fail — gate off (main `0ada588`, 2026-09-29) |

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
