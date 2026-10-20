# 0016. A feature is a chain of dependent todos

Status: Accepted
Date: 2026-10-15
Supersedes: none
Superseded-by: none

## Context

A feature that spans repositories arrives as one ask: the page shows what the
endpoint computes, and the endpoint needs the field the core crate adds. The
queue files one todo per ask, so the ordering lives nowhere. `todoq add` takes
`--blocked-by` at filing time, and nothing else: no step turns one feature ask
into the todos it really is, no view says which todo waits on which, and the
owner sequences the chain by hand every time.

Out of order, the cost lands on the gate. A web todo worked before its API todo
merged fails its gate against a route that does not exist yet; a consumer pin
bumped before the producer's tag is cut cannot build. Both read as a red gate
or a failed pin bump, far from the ordering mistake that caused them, and both
spend a full attempt before saying so.

Claim-time ordering also has to mean *landed*, not *done*. `done` is one
worker's verdict about a shift branch nobody has reviewed; a shift's pull
request can be held, reworked or closed. Work that reads a dependency's change
must therefore wait for the merge, with one exception: work that lands on the
same shift branch is reviewed together, so it may build on it.

Todo b4deca is the hands-off loop for one pull request — gate, review, land. It
is about a single change getting itself through. This record is about the order
between changes, which b4deca never sees. The 2026-10-15
`jeryu-whitepaper-brief` wiki page describes the queue as the owner's
sequencing tool; this is what makes that true without a new service.

## Decision

A feature that spans repositories is filed as several todos with explicit
dependencies, and the chain is the only scheduler.

`todoq decompose <family> "<ask>"` proposes the steps and their order — one
todo per repository change, each `after` the earlier steps whose work it reads.
It files nothing. The proposal is a draft file the filer reads and confirms
with `todoq decompose <family> --confirm <draft>`, which is the only step that
writes todos, with `blocked_by` wired to the ids it mints. A proposed step may
only wait on an earlier one, so a confirmed chain cannot contain a cycle. The
model's output is a proposal, never a filed todo; triage is unchanged, and one
ask filed with `todoq add` stays one todo.

`blocked_by` is honoured at claim time against the dependency's landed state.
`todoq.chain` is the single place that decides: a dependency clears when its
work is on the base branch — `merged`, not merely `done` — unless it landed on
the very shift the waiting todo would land on. Families that land straight on
their batch branch have nothing to merge afterwards, so there `done` is landed.
Every path through a claim asks that question: the worker's selection, and a
hand `todoq claim`, which refuses and names the id unless the person passes
`--force` (recorded in the todo's note). `todoq add --blocked-by` refuses an id
that is not in the queue, because a todo waiting on a typo waits for ever.

The same module phrases the reason, so `todoq list` prints `blocked on <id>`
with what that id is waiting for under each held todo, the worker reports it as
`todo.waiting_on_unmerged`, and jeryu says it on `/work`.

There is no scheduler service, no orchestrator and no queue of queues. The
chain is data on the todos; the workers that already poll the queue enforce it.

## Consequences

The ordering is now written down where the work is, so a wrong order is a
refused claim with a reason instead of a red gate an attempt later. The owner
confirms a chain rather than sequencing it, and the confirm is the only place
a chain enters the queue — a decomposition nobody read files nothing.

A chain is only as good as its merges. A dependency whose shift PR sits unmerged
holds everything behind it; that is the point, and `todo.waiting_on_unmerged`
and the `/work` reason are how it becomes visible rather than silent. The owner
still has to merge, and `--force` is still there for the person who means to
work out of order.

Decomposition costs a model call per feature ask and can propose the wrong
split; the draft step is where that is caught, and a proposal that cannot be
filed is dropped with a reason rather than repaired. A confirm that dies
between minting ids and the queue transaction leaves ids in no queue, and the
retry files exactly those; once they are in the queue a second confirm refuses.

Cross-repository order that cannot be expressed as "wait for main" is still out
of scope: a jeryu-core change consumed through a split tag needs the tag cut
between the two todos, so the chain can say "after", but not "cut the tag".
That remains a todo of its own.
