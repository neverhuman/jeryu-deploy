# 0001. The forge is origin; GitHub is a downstream mirror

Status: Accepted
Date: 2026-09-20
Supersedes: none
Superseded-by: none

## Context

Jeryu is a forge. Its repositories are also present on GitHub, and every tool
in the ecosystem — clients, CI integrations, an agent's muscle memory — assumes
that the GitHub copy is the real one. Left unstated, that assumption decides
the question by default: refs would be pushed to GitHub and pulled back, branch
protection would be whatever GitHub enforces, and merge authority would sit in
an account nobody here controls.

The forge already owns the properties that make a source of truth: the
protected repository on `git.neverhuman.org`, protected-ref enforcement in
`jeryu-gitd`, exact-head required contexts, and branch protection in
`jeryu-core`. Hosted-provider evidence, by contrast, is reachable only over a
network that can be slow, unavailable, or lying, and its absence is
indistinguishable from a pass unless something insists otherwise.

## Decision

The protected repository on `git.neverhuman.org` is the source and ref
authority for every jeryu repository. GitHub is a downstream mirror and never
an input.

- A merge happens on the forge. When a pull request merges into a default
  branch, the merge handler pushes the live tip to `github.com/<github_slug>`
  (`crates/jeryu-api/src/github_mirror.rs`). The flow is one-way: nothing is
  pulled back.
- A mirror target is explicit. A split-manifest `[[repo]]` is mirrored only
  when it carries `github_slug`, `jeryu_slug`, and `mirror_github_main = true`.
- A mirror push failure never fails the merge. The outcome is recorded as a
  `jeryu/github-mirror` check-run on the merged tip, and the attention inbox
  raises `mirror_failing` (`docs/pipeline-events.md`) so a human sees it.
- Hosted-provider data is optional read-only evidence. It degrades as
  `missing`, `stale`, `queued`, `failed`, or `unknown` and never becomes an
  implicit green signal (`docs/architecture.md`).

## Consequences

- Merge authority, branch protection, and required checks are enforceable
  locally and keep working with no route to GitHub at all.
- A mirror can silently fall behind the forge. That is accepted, and the cost
  is paid by the `jeryu/github-mirror` check-run plus the `mirror_failing`
  inbox item; the mirror's state is never read back as truth.
- Anyone reading GitHub — a person or a tool — may be reading a stale tree. A
  question about what is merged is answered by the forge.
- Mirroring needs a credential on the forge host (the SSH rewrite and deploy
  key), which is an operational surface the forge would not otherwise have. It
  grants push to the mirror only; it grants nothing over forge truth.
- Tools written against GitHub keep working, because the forge speaks a
  GitHub-compatible API rather than pointing them at GitHub. See [0002](0002-html-url-is-jeryu-shaped.md).
