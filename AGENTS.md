# jeryu-deploy Agent Instructions

This is a Jeryu split repository seeded from `cbecf7caa0e932c76a341b2521e66e911233860d`.

Before editing, read `README.md`, `agent/owner-map.json`,
`agent/test-map.json`, `agent/generated-zones.toml`,
`agent/proof-lanes.toml`, `agent/audit-policy.toml`, and
`agent/boundaries.toml`.

Keep split `main` clean. The legacy monorepo is
deprecated and archived as `jeryu/jeryu-monorepo`; this split family is the
only source of truth. Land changes through PRs with green required checks.

Canonical agent-readable detail is routed through `docs/architecture.md`,
`docs/boundaries.md`, `docs/testing.md`, `docs/generated-zones.md`, and
`docs/audit-rubric.md`. Deploy's release proof is its mapped standalone lanes;
the monorepo-only `jeryu-mapcheck docs` marker check is not a Deploy gate.

Cross-repo Rust dependencies are pinned to the exact immutable v5 tags and
commits recorded in `Cargo.lock`. Historical source spellings remain part of
Cargo package identity, but CI must transport them through the exact
`git.neverhuman.org` mappings in `.cargo/hosted-gitconfig`; committed or
release-CI sibling path patches are not permitted.

## Landing a pull request

- A PR is frozen once it is approved, or once its required checks are green and
  a human has marked it ready. After that, push only to fix a failing required
  check, answer a review comment, or make a re-pin or rebase the merge needs.
- Other work, including extra tests and findings made while landing, goes in a
  new PR from `main` that links back to the original.
- One writer per branch at a time. Before pushing, compare
  `git ls-remote origin <branch>` with the head you last fetched. If it moved,
  stop and fetch.
- Leave a human's PR state alone. Do not convert a PR they marked ready back to
  draft. If merging it is unsafe, say why once in a comment.
- Re-read live PR heads, merged flags, and tags before each landing step.
- Do not edit `CHANGELOG.md` in a change: release notes are generated at release
  time from commit subjects and `Todo:` trailers.

## Taking over another writer's branch

Whose branch it is matters less than whether its writer is still working on it.

- **Not yours to take: open a replacement PR.** Two kinds of branch are never
  taken over in place, whatever their writer is doing:
  - `contrib/gh-*`: the GitHub importer owns these. Every import pass
    force-resets the branch to the contributor's head and moves the PR back to
    it, so commits pushed there are orphaned within minutes.
  - A branch in a fork you cannot push to.
- **Active writer: leave the branch alone.** A branch is in use while any of
  these holds:
  - a todoq claim or lease on a todo that lands on it;
  - its shift is still taking work;
  - an agent session on its repository and branch is `active`, `idle` or
    `stale` in `todoq agents list` (a stale session may only have missed a
    heartbeat);
  - something pushed to it in the last 2 hours;
  - a person marked its PR draft or on hold;
  - its PR is frozen (see Landing a pull request: approved, or green with a
    human's ready mark). A push would void that review, so the freeze rule wins.

  Comment on the PR instead. If the work cannot wait, open a replacement PR
  from `main` (`git cherry-pick -x`) and close the old one with a link to the
  new one.
- **Finished writer: take it over in place.** A branch is finished only on
  positive evidence: its writer said it is done or handed it over, or its PR
  says so (a todo marked done with nothing left to land). The absence of the
  signals above is not enough on its own. Then:
  1. Say so first, in a PR comment: "Taking over at <sha read>: <why the
     writer is done>".
  2. Read the head with `git ls-remote origin <branch>` and work from exactly
     that sha.
  3. Just before pushing, check every in-use signal above again. If one has
     appeared, stop and comment instead.
  4. Add commits on top with a plain push. To rebase onto `main`, first check
     that every commit came across (`git range-diff <sha read>...HEAD`), then
     push with
     `git push --force-with-lease=refs/heads/<branch>:<sha you read> origin HEAD:<branch>`.
     It refuses if anyone pushed since you read the branch. Never a bare
     `--force`, and never on `main` or another protected branch.
  5. Follow up on the PR with the sha you replaced and the sha you pushed, so
     the old head can be recovered.
  6. The PR keeps its number and its review thread. On the push, the forge
     records the head's merge base with `main` as the PR's base (#401), so the
     diff shows only this PR's changes, and review runs again on the new head.

## Files every PR touches

- A file that many open PRs edit at the same spot conflicts on every merge
  queue replay, and a refused replay costs a replacement PR. Append-only
  tables carry `merge=union` in `.gitattributes` (the migration README does).
- When a change adds tests to a large shared test file (for example
  `crates/jeryu-api/src/web/tests/pulls_tests.rs`), put them in a new file by
  topic instead, and move that topic's existing tests with them when it is cheap.

## Numbering a migration or an ADR

- `db/migrations/` and `docs/adr/` are numbered sequences. Take the next
  number from current `origin/main` at the time you land, not from your
  branch's base: a branch cut from an older `main` sees numbers that are no
  longer free. Fetch, look at `origin/main`, and renumber before the PR goes
  ready if `main` has moved past you.
- Renumbering is fine until the file lands; after that a number is never
  reused or changed. `crates/jeryu-api/tests/migration_numbers.rs` and
  `crates/jeryu-api/tests/adr_records.rs` fail on a shared or skipped number.
  A landed file is never deleted: supersede an ADR, or land a migration that
  undoes an old one.
