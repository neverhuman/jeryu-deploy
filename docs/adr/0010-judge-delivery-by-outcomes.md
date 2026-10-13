# 0010. Judge delivery by outcomes

Status: Accepted
Date: 2026-10-13
Supersedes: none
Superseded-by: none

## Context

Flow shows where work waits and Yield shows what recorded work cost, but neither answers whether the factory delivers changes that last, recovers quickly, or checks the ideas it ships. Activity counts alone reward busy systems even when changes fail, get reworked, or never reach production. Existing events prove some transitions; defects, incidents, human effort and hypothesis outcomes need new evidence sources.

## Decision

jeryu gets an admin-only **Scorecard** page at `/scorecard`, in the system navigation, with 7, 30 and 90 day windows and family and repository filters. Monthly outcomes have a separate tab. Every tile shows its value, the immediately preceding equal-length window, a trend line, a link to the Flow lot, todo or pull request rows behind it, and a concise measurement note. Missing evidence is shown as `unknown` with its reason, never as zero, an estimate presented as fact, or fixture data.

The scorecard judges outcomes: lead time, escaped defects, recovery, review findings, and cost per successful change. Volume is context only and every volume tile is paired with a quality guardrail. The complete metric contract, formulas, source availability and edge cases live in [`docs/scorecard.md`](../scorecard.md).

Measures use event timestamps and the existing `flow_ledger`, `cost_ledger`, and `pipeline_events` where those prove the needed facts. Outcome and incident ledgers, review finding outcomes, main health, line survival, human time and a hypothesis field are separate follow-up sources. Metrics depending on them remain unknown until they exist; no backfilling from guesses is allowed.

## Consequences

- Outcome quality is visible beside speed and cost, along with the limits of current instrumentation.
- Initial releases have unknown tiles; source work must land before those tiles can report.
- Pairing activity with quality makes high output legible without treating output itself as success. Cost and human time use successful changes as the denominator, not merged work.
- The scorecard is a read model, not a new authority for lifecycle facts. Durable new facts belong in auditable ledgers.
