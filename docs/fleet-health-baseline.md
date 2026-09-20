# Fleet health baseline

`GET /api/v1/control-plane/status` is the number any new gate's output gets read
against. A gate that adds twelve failures to a backlog of 268 is invisible
unless the 268 is understood first. This note records what each headline field
on that endpoint actually measures, so a reader can tell a regression from a
constant.

Triaged 2026-09-20 against the `nightshift/2026-09-19` shift branch.

## `failingCheckCount` is a count, not a diagnosis

The summary used to carry only the total, and the `ci-failing-checks` priority
listed an arbitrary first five check runs. Neither answered the only question
that decides what to do next: is this one lane failing on every head, or many
independent breaks?

`summary.failingCheckCauses` now groups the failing check runs by check name and
conclusion — the pair a repair actually targets — largest first, with each
cause's share of the total and the repositories it spans. The
`ci-failing-checks` priority leads with the dominant cause and uses the grouped
lines as its evidence.

Read it this way:

- one cause at a high share: one repair, and the backlog is a single incident.
  Baseline a new gate against the *remainder*, not the total.
- many small causes: the total is the noise floor. A new gate's failures have to
  be separated by name before they mean anything.

Causes are computed over the same active view as the rest of the summary —
check runs on open-PR heads only — so failures left on merged or closed heads
never inflate the baseline.

## `mirrorState` and `artifactState` are fixed defaults

Both report `missing` unconditionally. `remote_status()` takes no arguments and
`artifacts()` ignores the web state entirely; see
`crates/jeryu-api/src/web/control_plane/model.rs`. The value means "no evidence
adapter is wired for this subsystem", not "evidence that used to be here went
away".

Consequences for triage:

- neither field can regress, and neither can recover in response to an unrelated
  repair. Changes elsewhere in the forge do not move them.
- the two `artifacts-latest-missing` and `github-mirror-missing` priorities are
  therefore permanent fixtures of every snapshot, not incidents. They are
  standing work items for wiring the adapters.
- `artifacts.absenceIsSuccess` is `false` on purpose: absent artifact evidence
  must not be read as a passing release check.

Each response carries a `reason` string naming which adapter is unwired. Read
that before opening an investigation into the state itself.

## `jeryu-runner-*` containers are not the runner fabric

The host carries a large pool of long-running `jeryu-runner-<uuid>` containers
(`gitlab/gitlab-runner`). They are unrelated to the runner counts this endpoint
reports, and they perform no work:

- each is configured against a GitLab instance that no longer accepts
  connections, and retries `POST /api/v4/jobs/request` every three seconds
  forever. Sampled containers show `builds: 0` and nothing but that failed poll
  since their last restart.
- all of them share one registration token, so they are one registration, not a
  pool with capacity.
- they never contact the jeryu forge. `runnerState` is derived from gate-runner
  heartbeats recorded by the forge (`control_plane::runner`), so these
  containers contribute no nodes, no slots, and no check runs. They cannot be
  any part of `failingCheckCount`.

They are pure host noise: log volume and a steady connection-retry rate. Removing
them changes nothing the control plane reports, which is exactly why it is safe —
but it is a host operation, outside any repository, and needs a human to confirm
the GitLab instance is not coming back before the registration is dropped.
