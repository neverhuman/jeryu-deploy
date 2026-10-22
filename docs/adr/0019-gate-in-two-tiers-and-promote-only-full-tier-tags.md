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

**Two tiers.** Every gated repository carries two tiers of its gate:

- the fast tier (`required-fast`): format, lint, the smoke-feature unit tests,
  pin, lock and dependency-source checks, and any check whose failure would make
  the artifact unbuildable;
- the full tier (`required`): everything it holds today.

The full tier includes every fast-tier check plus additional checks, using
shared check definitions. Changes to tier membership require explicit review and
full-tier validation before taking effect.

The gate posts `<repo>/fast` and `<repo>/required` as separate contexts. Fast
and full results bind the same source and environment inputs, but their
evidence and reuse keys distinguish the recipe and status context. A fast result
cannot satisfy a full-tier requirement.

**When each tier runs.**

- Main-bound changes require fast checks followed by the remaining full checks.
  A fast failure is reported in minutes, and the full checks are not started for
  that head.
- Release-line changes require fast checks only. Release tags trigger full
  validation.
- Existing full results may be reused when the complete evidence identity
  matches.
- Tag validation runs in the background at a lower priority than merge checks
  and development work, so it cannot starve them.

Which tier a branch requires is set where protection is enforced today: the
forge's branch protection for that branch, whose required contexts only a
repository administrator may change. `main` keeps requiring `<repo>/required`.

**Promotion requires the full tier on the exact release candidate.** Release
tooling may publish a tag to a development channel once the tag's commit has
`<repo>/fast` from the gate identity. Staging and production require full-tier
evidence for the exact release candidate and the dependency revisions recorded
in its lock. Promotion selects the same built artifact, identified by digest,
rather than rebuilding it. The check is enforced at the deployment entry point,
manual deployment included, not only in a release bot. If the full tier fails on
a tag, the tag stays development-only, and its fix ships as a new tag. A tag's
evidence is never edited.

Promotion enforcement must be implemented and tested before release branches
switch to fast-only protection.

**No author attestations.** A gate result counts only when a registered gate
host posted it as the gate identity. An author's local run is debugging and
carries no weight in protection or promotion. Results are shared between gate
hosts through the existing evidence-identity reuse, and each owner is claimed by
exactly one host at a time.

**Find host and `main` faults before a release, not during it.** These are
useful on their own, whatever happens to the tiers:

- The gate runs `--main-health` on every gated `main` on a schedule. A red
  `main` is reported to its owner as itself, not discovered as a failure on
  someone's change.
- Tools the gate pins by digest are held at their reviewed version on gate hosts
  and upgraded on purpose. When one is upgraded, the new reviewed digest lands
  before the upgrade, recorded beside the package version it belongs to. A
  security fix for an actively exploited tool may be installed first. Its digest
  then lands within one working day, and that tool's lane fails closed until it
  does.
- Before a release chain starts, a preflight reports stale slot locks,
  pinned-tool drift and red family mains in one pass.

**Overlap only what is immutable.** Downstream pins continue to use landed
upstream commits accepted by the existing pin verifier. Preparation and builds
may overlap where their inputs are immutable, for example compiling from a pushed
tag before the deploy lock that names it lands. A changed input invalidates the
corresponding evidence. Only publication waits for the lock.

**Roll out on one release line first.** Start with one existing development
release line. Measure four things:

- development release elapsed time;
- gate queue wait;
- total gate compute;
- full-tier failures.

Expand tier adoption and add host capacity when these measurements justify it.

## Consequences

- On the pilot release line, each development release gates in minutes rather
  than tens of minutes per repository. Downstream pins still wait for upstream
  commits to land, so the chain stays ordered, but each link is shorter.
- A development channel can run a tag that the full tier later rejects. That is
  the price, and it is paid only in development: staging and production never
  see such a tag. Owners must expect an occasional development-only tag and its
  follow-up.
- Every repository maintains tier membership over shared check definitions.
  A check that only the full tier runs can rot on release lines between tags.
  The scheduled main health and the tag validation are what catch it.
- Promotion enforcement becomes a prerequisite for fast-only protection, so the
  order of work is fixed: the promotion check first, then the protection change.
- Implementation requires updating pr-redteam to read the branch's enforced
  required contexts. This proposed record does not itself change review policy.
- `docs/merge-queue.md` documents `<repo>/required` on the exact sha as an
  invariant. It changes only for release lines, and only when their protection
  changes.
- Gate hosts hold pinned tools, so ordinary upgrades of those tools need a
  deliberate step: land the reviewed digest, then upgrade. That is slower than
  unattended upgrades and is the point. The security exception above keeps it
  from delaying an urgent fix.
- Preflight, pinned tools and scheduled main health pay off without the tiers.
  They are not blocked on the rest of this record.
