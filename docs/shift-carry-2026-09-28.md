# Carrying dayshift/2026-09-28 onto main

`dayshift/2026-09-28` never got a review PR, and `dayshift/2026-09-30`
(jeryu-deploy PR #107, jeryu-web PR #72) landed first and rewrote part of the
same code. The 09-28 work was therefore re-applied commit by commit on top of
main instead of merged, and the branch can be deleted once the table below is
true of main.

## jeryu-deploy

| 09-28 commit | subject | where it is now |
| --- | --- | --- |
| `1694ea4` | Show a pull request's commits in the GitHub edge | re-applied on this branch (cherry-picked; see the `cherry picked from` trailer) |
| `b06dcc7` | Bound post-push jankurai audits to one queue | dropped — main's `e7d46cc` ("Score jankurai on the gate runners, not on the forge host", PR #107) scores off the forge host, so there is no post-push audit on the forge to bound |

## jeryu-web

| 09-28 commit | subject | where it is now |
| --- | --- | --- |
| `1cef325` | PR checks: every row says why it is red and whether it blocks the merge | re-applied on the matching jeryu-web branch |
| `e327a22` | Show a pull request's commits on the PR page | re-applied on the matching jeryu-web branch |

## Forge systemd unit limits (todo `20260928-…-cb74fc`)

The todo asked to also carry the forge unit's `MemoryMax`/`TasksMax` from
`762b4c2` and to adapt or drop that commit's per-audit cgroup cap, since audits
no longer run on the forge. `762b4c2` is not reachable from any ref in this
repository or any other repository of the family, and no `MemoryMax`/`TasksMax`
setting exists anywhere in their history, so there was nothing to carry: the
forge's own unit is not a file this repository tracks (the units it does ship
are under `scripts/release/systemd/`, `scripts/release-board/systemd/` and
`ops/security/`). The per-audit cgroup cap is moot for the same reason `b06dcc7`
was dropped. A human who still wants the host limits has to re-create them on
the forge host, or push `762b4c2` somewhere this family can see it.
