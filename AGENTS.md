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
- One writer per branch. Before pushing, compare `git ls-remote origin <branch>`
  with the head you last fetched. If it moved, stop and fetch. Never force-push
  a branch someone else has pushed to: open a replacement PR from `main`
  (`git cherry-pick -x`) and close the old one with a link to the new one.
- Leave a human's PR state alone. Do not convert a PR they marked ready back to
  draft. If merging it is unsafe, say why once in a comment.
- Re-read live PR heads, merged flags, and tags before each landing step.
