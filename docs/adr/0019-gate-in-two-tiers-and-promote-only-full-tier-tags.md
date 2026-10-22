# 0019. Gate in two tiers and promote only full-tier tags

Status: Proposed
Date: 2026-10-22
Supersedes: none
Superseded-by: none

## Context

Every change runs one gate, the repository's `required` recipe, whatever it is
for. That recipe is the whole bar: format, lint, unit tests, a coverage pass
that builds and runs every test again, document and visual lanes, real-feature
builds and cross-repository authority checks. A dependency carry on a release
line, or a cosmetic change meant only for a development environment, waits for
the same 20–40 minutes per repository as a change bound for customers.

The cost adds up along a pin chain. A release that touches a core crate, a
report crate, a web crate and a deploy lock gates four repositories in order,
and each held review or upstream fix re-gates everything downstream. One
development release in October 2026 took more than eight hours of wall clock
for about an hour of real change. Most of it went in three ways:

- re-gating downstream pins each time an upstream head moved;
- gating the same head twice, once on the author's machine and once on the gate
  host;
- finding, one round at a time, faults that already sat on the gate host or on
  `main`: a stale lock file in a slot tree, a distribution upgrade of a pinned
  tool, a release line whose test expectations `main` had already fixed, and a
  lock file that had drifted from its siblings' mains.

Two shortcuts suggest themselves, and both are wrong as stated.

**Accept the author's local run.** The hosted gate exists because results
produced on another machine's toolchain are not results for the gate host. The
runner re-proves manual statuses for exactly that reason. In that same release
the author's runs passed while the gate host failed twice: once on a step the
author's machine had to skip, and once on a tool the gate host had upgraded
that morning. The gate identity must also never be the author of what it
proves, and a certificate signed by the author's own tooling breaks that.

**Lower the bar for everything.** That ships unproven code to customers.

The runner already has the mechanism a sound version needs. Since pr-gate 2.0.0
a head's result is keyed by an input identity, covering the repository SHAs,
sibling mains, recipe PATH and host environment. A result whose identity matches
is reused rather than re-run, including a result posted by another gate host on
byte-identical inputs.

## Decision

**Two tiers.** Every gated repository carries two recipes:

- `required-fast`, the fast tier: format, lint, the smoke-feature unit tests,
  pin, lock and dependency-source checks, and any check whose failure would make
  the artifact unbuildable.
- `required`, the full tier: everything it holds today.

The fast tier must be a strict subset of the full tier. A check may move from
full to fast, never the reverse, and a repository test asserts the subset
relation. The gate posts `<repo>/fast` and `<repo>/required` as separate
contexts from the same input identity, and it runs the fast tier first, so a
fast failure is reported in minutes and the full tier is not started for that
head.

**Which tier a branch requires.**

- `main` keeps requiring `<repo>/required`.
- A release line (`release/*`) requires `<repo>/fast`. Its merges feed
  development releases only. Its tags are what get promoted.

**Promotion requires the full tier on the exact tag.** When a release tag is
pushed, the gate runs the full tier on the tagged commit and posts
`<repo>/required` there. Release tooling may publish a tag to a development
channel once the tag's commit has `<repo>/fast` from the gate identity. It
refuses to promote any tag to a staging or production channel unless the tag's
commit has `<repo>/required` from the gate identity. If the full tier fails on
a tag, the tag stays development-only, and its fix ships as a new tag. A tag's
evidence is never edited.

**No author attestations; more gate hosts instead.** A gate result counts only
when a registered gate host posted it as the gate identity. An author's local
run is debugging and carries no weight in protection or promotion. Capacity
comes from registering more gate hosts. Their results are shared through the
existing input-identity reuse, so one host's run satisfies another host's claim
on identical inputs, and each owner is claimed by exactly one host at a time.

**Find host and `main` faults before a release, not during it.**

- The gate runs `--main-health` on every gated `main`, the fast tier hourly and
  the full tier nightly. A red `main` is reported to its owner as itself, not
  discovered as a failure on someone's change.
- Tools the gate pins by digest are held at their reviewed version on gate
  hosts and upgraded on purpose. When they are upgraded, the new reviewed digest
  lands before the upgrade, recorded beside the package version it belongs to.
- Before a release chain starts, a preflight reports stale slot locks,
  pinned-tool drift and red family mains in one pass.

**Release builds overlap the lock.** A release build may start compiling from a
tag before the deploy lock that names it has landed. Only publication waits for
the lock.

## Consequences

- A development release of a pin chain gates each repository in minutes rather
  than tens of minutes, and its downstream pull requests can be opened against
  upstream heads that are frozen, because the forge keeps the head SHA on merge.
- A development channel can run a tag that the full tier later rejects. That is
  the price, and it is paid only in development: staging and production never
  see such a tag. Owners must expect an occasional "dev-only" tag and its
  follow-up.
- Every repository maintains two recipes and the subset test. A check that only
  the full tier runs can rot on release lines between tags. The nightly
  main-health full run and the tag gate are what catch it.
- pr-redteam must read the tier a branch requires. A release-line carry with a
  green `<repo>/fast` is complete gate evidence for that branch, not a missing
  `<repo>/required`.
- Gate hosts hold pinned tools, so security upgrades of those tools need a
  deliberate step: land the reviewed digest, then upgrade. That is slower than
  unattended upgrades and is the point.
- Registering a second gate host is now on the critical path. Until it exists,
  the tiers cut the time per gate, but one host still serialises every family's
  queue.
