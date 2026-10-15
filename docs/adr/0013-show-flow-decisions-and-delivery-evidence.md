# 0013. Show Flow decisions and delivery evidence

Status: Accepted
Date: 2026-10-15
Supersedes: 0004
Superseded-by: none

## Context

A single ordered station row suggests that review precedes checks and that a
consumer pin avoids a second pull request. Timer waits were reported as active
work. Production deployment events without a PR key could not close Flow lots.

## Decision

Show parallel review and required checks, explicit decision outcomes, repository
handoffs, and the intentional activation hold. Station details explain purpose,
triggers, failure paths and responsible services. Daily UTC bars count durable
transitions, with drill-down to items and recorded execution identities.

Correlate delivery only through the immutable release revision and Git ancestry.
The Jeryu deploy release also carries the exact web revision in its checked-in
lock. Do not infer membership from merge time or current main. Keep receipts and
station transitions beyond event-log retention. A timer wait is not execution.

## Consequences

Migration 0020 preserves measured historical transitions; it cannot reconstruct
historical holds or execution metadata that was never retained. The UI exposes
that coverage boundary. Membership reads are bounded and cached per event; absent
Git objects leave work unresolved. Other consumer and installation routes need
explicit artifact contracts before they can be declared shipped. The diagram
shows those routes as unspecified rather than inventing them. Runner pools are
not evidence that any particular runner handled an item.
