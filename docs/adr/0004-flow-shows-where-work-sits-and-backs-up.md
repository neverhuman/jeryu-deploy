# 0004. Show where work sits and backs up as one Flow view

Status: Superseded
Date: 2026-10-09
Supersedes: none
Superseded-by: 0013

## Context

The forge runs a whole delivery line: a todo is filed, a worker slot claims and
works it, the work lands on a shift branch, a pull request is opened, reviewed,
gated, merged (directly or through the merge queue), pinned into the repository
that ships it, staged and deployed. Each step has its own tool: worker slots,
the reviewer, gate-runner slots, the merge queue, the release timers, a person
for the deploy.

Every piece of that is already observable, one piece at a time. The pipeline
event log records the transitions (`todo.*`, `worker.stage`, `pr.*`,
`gate.started`/`gate.finished`, `review.started`/`review.finished`, `queue.*`,
`pin.bump_opened`, `release.staged`, `deploy.*`). `GET /api/v1/trace` answers
where **one** todo or pull request is, across twelve fixed stages. `/runners`
shows each runner's heartbeat. `/yield` says what the work cost.

What nobody can see is the line as a whole: how much work sits at each step,
how long it waits there versus how long the step itself takes, which step is
the bottleneck, where work loops back (a red gate, a held review, a refused
queue replay, a killed run), and whether each step has the capacity it is
supposed to have. In practice the time goes to waiting, not to work. A two-line
pin bump spent 16 minutes from open to merge, of which the gate ran 6. A
merge-pass trigger failed silently for two days because nothing showed that
one step had dropped from a 30-second cadence to 5 minutes.

Semiconductor fabs solve the same problem with WIP per step, queue time versus
process time, rework loops and holds, all laid out along the route. The
concepts carry over directly.

## Decision

jeryu gets one page, **Flow** (`/flow`), that lays the delivery line out as a
route of **stations** and shows it in two modes from one model:

- **Live**: what is at each station now (waiting, active, blocked), the oldest
  wait, queue time and process time (median and 90th percentile), throughput,
  rework loops back along the line, holds, and the bottleneck.
- **Layout**: the same stations with the runners that serve each one, their
  configured versus reporting capacity and their health.

The stations are the stages `GET /api/v1/trace` already defines, grouped into
the steps a person reasons about (the table in `docs/flow.md`). Flow is built
from the pipeline event log and runner heartbeats only. A station or number the
forge cannot prove is shown as unknown, never as healthy, and nothing on the
page is a fixture.

A durable **flow ledger** (one row per piece of work per station: entered,
started, left, outcome) is projected from the event log as events are stored,
so history outlives the log's 30-day retention. The same ledger answers
per-stage estimates (an ETA for every in-flight piece of work), which therefore
live in Flow's model rather than in a separate estimator.

Activity stays the raw event stream and links each event to its station;
Runners links to Layout; Yield stays the cost view. Flow does not duplicate
them.

## Consequences

- One model behind both modes means the operational view and the
  configuration view cannot drift apart. The price is that both wait on the
  ledger: neither ships before the projection does.
- The ledger is a second derived store beside the cost ledger. It must be
  rebuildable from the event log (within retention) and is versioned with its
  own migration, so a projection bug is fixed by a replay, not by hand edits.
- Stations whose events do not exist yet start as unknown: when a gate starts
  waiting for a slot (no event before `gate.started`), and intake before a todo
  is filed (no ADR or feature objects exist yet). Those gaps are visible on the
  page instead of being papered over, and closing them is follow-up work.
- Site specifics stay out: station tools are named by runner label and id as
  the heartbeats report them; expected capacity is site configuration, absent
  by default and shown as unknown when absent.
- The page invites comparison across families and over time. The first
  version reports a window (7, 28 or 90 days) and the live state, without
  alerting thresholds; alerts belong in the attention inbox once the numbers
  have a baseline.
