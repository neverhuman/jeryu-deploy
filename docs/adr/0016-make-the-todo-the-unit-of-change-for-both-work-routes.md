# 0016. Make the todo the unit of change for both work routes

Status: Accepted
Date: 2026-10-15
Supersedes: none
Superseded-by: none

## Context

A Jeryu forge serves a fleet pulling todos from a queue and people driving
their own agents: Claude Code, Codex, `gh`, MCP clients and sandboxed
web-terminal sessions. Both already deliver pull requests to the forge.
Records 0005–0009 give personal agents scoped tokens, presence, bot kinds and
person or organization ownership. The Shift API, `Todo:` trailers and the
pipeline event log give fleet work a trace. Ad-hoc session cost already appears
on `/yield` beside fleet cost; cost visibility alone does not join a session
to a delivered change.

The tracking model has lived in the todoq README and wiki decision 27, rather
than a durable family ADR. Without it, a Work Tracker row can be mistaken for
the queue authority, and a fleet-only outcome denominator can be mistaken for
all changes. Record 0010 and the [Scorecard contract](../scorecard.md) require
successful changes, rather than activity or merely merged PRs, as the cost
denominator.

## Decision

1. **Support both routes through the same endpoint.** A fleet may pull work
   from the queue, and a person may drive an agent directly. Both submit PRs
   to the same forge checks, quality gate, review and merge endpoint. Neither
   route grants a bypass, including a session launched by the forge itself.
   Agent identity and queue ownership do not confer merge authority.
2. **Keep public code and private queues.** A todo is a Markdown file with
   TOML frontmatter in a private per-family queue repository, on its `queue`
   branch, outside the public product repositories. Todos name hosts and
   people; publishing product code does not publish its operational queue.
3. **Use Git as the only claim authority.** The forge rejecting a stale push
   to the queue branch is the only lock. Claims, lease renewals, releases and
   other queue-state transitions are commits to that branch. A local edit or
   a rejected push is not a claim; after a collision the writer rereads the
   branch before trying again. Lease expiry is evaluated from committed state,
   not a second claim registry. There is no SQL mutex or lock server deciding
   ownership independently of the queue.
4. **Join the change by todo.** A change is one todo plus the pull requests
   that carry it, potentially across repositories, joined by the `Todo:`
   trailer used by the outcome model of ADR 0010. Several PRs for one todo
   remain one change. Because merges rebase, merged status is computed from
   ancestry or that trailer on main, not solely from the original branch SHA.
   Merged is evidence of integration, not proof of successful delivery.
5. **Keep decisions and projections in their places.** Decisions never live
   in todos: the durable record belongs in `docs/adr/`, following its
   [convention](README.md). The forge's Work Tracker (`jeryu-jira`), Shift API
   and pipeline event log are the read side of todo state, never its writer
   or a competing authority. They may record execution evidence and project
   queue commits; they do not acquire or extend claims in their own stores.
6. **Require a trace for personal work to be the same unit.** A personal-agent
   change counted as a todo change must name its private family todo through
   `Todo:` on the carrying commits, retain that association through merge, and
   link the participating session and authenticated bot identity, including
   its kind and responsible person (or organization for a team bot). A
   `Session:` trailer is the intended session join. Public trailers carry
   opaque identifiers, not private todo contents, host names or credentials.
   A PR explicitly marked as having no todo may still take the same gate,
   but is not thereby a todo change and must not silently enter the todo
   denominator. A missing trailer is missing evidence, not an implicit
   no-todo declaration.

This record accepts the tracking model and the evidence requirement. It does
not claim that personal-agent metadata validation or collection is implemented.
The exact no-todo marker syntax, validation of `Todo:` and `Session:` for
personal PRs, and how the bot kind is retained with the evidence remain to be
specified and enforced. Whether a personal agent may claim an existing queued
todo, and how that claim is attributed to the person, also remains undecided.
Any such claim facility must use the queue-commit authority above; possession
of a scoped forge token alone is not queue claim permission.

## Consequences

- People can work interactively without joining a fleet, while reviewers and
  branch protection apply the same delivery standard to both routes. This
  preserves the portal README's “Two ways to work” premise; it adds no third
  route or shortcut requiring that section to change.
- Queue state can be reconstructed from Git without reconciling a database
  lock with a lease. Read-side lag is possible; a displayed claim is a
  projection and a writer must still win the queue push.
- Two existing tracking gaps remain explicit follow-up work: `blocked_by` is
  honoured at filing, not at claim (todo **984b89**), and acceptance criteria
  are free prose rather than a machine-checked contract (todo **4b0aad**).
- Personal PRs are not yet measured like fleet work merely because they have
  an identity or session cost. The trailer, session, bot-kind and person
  attribution contract needs implementation before claiming equal coverage.
  Outside-agent contributed PRs currently get no equivalent audit proof;
  whether and how they receive it remains open in the contributed-PR todos.
  Passing required checks must not be described as proof that this missing
  audit evidence exists.
- The Scorecard's successful-change denominator must cover both fleet and
  personal routes, deduplicate the PRs of a todo, and apply the same success
  criteria. Until the joins exist, it must name the routes and no-todo work
  it excludes and show missing evidence as unknown. `/yield` including both
  costs does not justify dividing that total by fleet successes alone without
  disclosing the mismatch. This record clarifies coverage under ADR 0010;
  it does not replace its outcome definitions.
