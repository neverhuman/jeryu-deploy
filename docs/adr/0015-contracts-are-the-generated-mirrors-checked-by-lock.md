# 0015. Contracts are the generated mirrors, checked against a lock

Status: Accepted
Date: 2026-10-15
Supersedes: none
Superseded-by: none

## Context

The family's cross-repository interface is the pinned `*-split.N` tags plus
two generated TypeScript mirrors: the wire types the web app compiles against
(produced from the read-model crate in jeryu-core, copied into jeryu-web's
`contracts/generated/`) and the work-tracker types (produced by jeryu-jira's
own crate into its `contracts/generated/`). A change to the REST edge or to a
DTO could merge in the producer while a consumer still compiled against the
old mirror, and the break surfaced at pin bump time, after the fact. The rule
"API and schema changes require versioned contracts and compatibility checks"
had no concrete answer.

An OpenAPI document for `/api/v1` and `/api/v3` was considered. Neither edge
has a machine-readable description today, the typed DTOs are already the
source of both the server and the mirrors, and a second hand-kept or
separately generated description would be one more thing that can drift.

## Decision

The contract artifact is the generated mirror: the ts-rs output committed in
the repository that consumes it, and in jeryu-jira's case in the producer.
No OpenAPI document is introduced now; it can be added later as a further
generated artifact under the same lock rule without superseding this one.

Each repository holding a mirror also holds `contracts/generated.lock`: the
sha256 of every mirror file as of the last reviewed sync, written by
`ops/ci/contract_compat.sh --write`. The check, `ops/ci/contract_compat.sh`,
runs in that repository's required gate (jeryu-web and jeryu-jira) and fails
when the mirror and the lock differ. Its message names the producer and the
consumer and classifies each file:

- removed file: breaking;
- changed file: breaking unless only fields were added, which the reviewer
  confirms from the diff;
- added file: additive.

Breaking means: a type or field removed or renamed, a field's type changed or
narrowed, an optional field made required, an enum variant removed, or a route
path, method or status code on the REST edge changed or removed. Adding an
optional field, a variant clients may ignore, or a route is additive.

A breaking change lands as a new additive shape first and the old one is
removed in a later change, after every consumer named in the failure message
has moved. The lock moves in the same commit as the mirror it describes, so
the diff of the lock is the reviewed record of the interface change.

The producing repository's gate is expected to run the same lock check against
its own export, so a break is caught before the producer merges rather than at
the consumer. jeryu-jira does this now. jeryu-core's export of the web mirror
is not covered by this record's change set: core is consumed by split tag and
changes in its own todo.

## Consequences

A mirror can no longer drift or be hand-edited without a visible lock diff and
a gate failure that names the consumer. The lock detects change, not meaning:
classifying a changed file as additive stays a reviewer's judgment. Until
jeryu-core runs the lock check on its export, a producer-side break is caught
when the mirror is next synced into jeryu-web, not when core merges. REST
route-level compatibility (paths, methods, status codes) is covered only
through the DTOs; a route inventory remains the hand-kept fixture in jeryu-web
and is not locked here.
