# Architecture decision records

A decision that outlives the work that produced it belongs here. Todos are
deleted when they are done, so a decision recorded only in a todo is gone the
moment the work lands; a record here stays, with the reasons and the price.

## Where a record lives

- Cross-cutting family decisions — anything a second repository has to obey —
  live in `jeryu-deploy/docs/adr/`. Deploy is the release authority and already
  holds `docs/architecture.md` and `docs/boundaries.md`.
- A decision that binds exactly one repository lives in that repository's own
  `docs/adr/`, numbered in its own sequence.

## Shape

One file per decision, named `NNNN-kebab-title.md`, `NNNN` a zero-padded
four-digit number that is never reused and never renumbered. The file starts
with an `# NNNN. Title` heading, then the header lines, then four sections in
this order:

```markdown
# 0007. Title in the imperative

Status: Accepted
Date: 2026-09-20
Supersedes: none
Superseded-by: none

## Context
## Decision
## Consequences
```

- **Status** is one of `Proposed`, `Accepted`, `Rejected`, `Superseded`.
- **Supersedes** and **Superseded-by** are always present, each either `none`
  or a comma-separated list of record numbers (`0003`). They are written out
  even when empty so that a reader never has to decide whether a missing line
  means "nothing" or "nobody wrote it down".
- **Context** is the world as it was: the forces, not the conclusion.
- **Decision** is what was chosen, in the present tense and active voice.
- **Consequences** are what the project now lives with, the costs as well as
  the benefits.

A record is never edited into a different decision. To change a decision, add
a new record, set the old one's `Status: Superseded` and `Superseded-by:` to
the new number, and name the old number in the new record's `Supersedes:`. The
two lines are kept consistent in both directions, which is what makes the
directory readable back to front.

`crates/jeryu-api/tests/adr_records.rs` checks the numbering, the header
lines, the section order, and that every supersession points both ways.
