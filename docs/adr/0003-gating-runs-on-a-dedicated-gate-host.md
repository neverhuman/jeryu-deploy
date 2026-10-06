# 0003. Gating runs on a dedicated gate host

Status: Accepted
Date: 2026-09-20
Supersedes: none
Superseded-by: none

## Context

A merge needs an exact-head required context to be green, so something has to
run the gate. Hosted runners were not an option that fits the rest of the
design: the forge is the source of truth ([0001](0001-forge-is-origin-github-is-a-downstream-mirror.md)), so a gate result minted
outside it would be evidence from a system that is downstream of the thing it
is judging, delivered over a network whose absence looks like silence rather
than failure.

The gate also wants a machine with a warm build cache. A cold `cargo` workspace
build is the difference between a gate that a reviewer waits for and one they
walk away from, and the cache is only warm if the same host keeps running the
same builds.

The forge itself runs on its own production host. Putting the gate there would have gate
builds competing with production for CPU and page cache on the host that must
stay responsive.

## Decision

Pull-request and merge-queue gates run on **one dedicated gate host** that the
operators own, on gate-runner slots.

- One runner process per slot (`pr-gate-runner.sh`), each with a stable id of
  the form `<host>/<slot>` — e.g. `build-1/slot0`. That id is the `actor` on pipeline
  events and the key the forge stores heartbeats under.
- A slot posts a heartbeat when it starts a gate, when it finishes one, and on
  every idle tick (once a minute). The forge keeps the latest report per runner
  and renders it on `/fleet` and `/runners`
  (`crates/jeryu-api/src/web/control_plane/gate_runners.rs`).
- A slot that stops reporting is shown offline after 180 seconds rather than
  being dropped, and the attention inbox raises `gate_runner_down` when no slot
  is online while a pull request is open or the merge queue is building.
- Only logins named in `JERYU_RUNNER_REPORTERS` (a site setting, e.g. `ci-bot,review-bot`) and
  forge admins may report, so an ordinary account cannot paint a fake runner.
  The contract is in `docs/pipeline-events.md#runner-heartbeats`.
- The gate host is also the release build host (`JERYU_BUILD_HOST`), which is what
  keeps the gate's build cache and the release build on the same toolchain and
  the same glibc.

## Consequences

- Gate capacity is one host's worth of slots. A queue of pull requests waits;
  it does not autoscale. That is the accepted price for gating on a machine
  we own, offline, with a warm cache.
- The gate host is a single point of failure for merging. The outage is visible rather
  than silent: slots go offline after 180 seconds and `gate_runner_down` is a
  `critical` inbox item.
- The host is long-lived and stateful, so gate results depend on its installed
  toolchain. Anything that must be reproducible elsewhere is pinned — the
  builder image tag, the Rust toolchain, `cargo --locked --offline`.
- Production on the forge host is not slowed by gate builds.
- Adding capacity means adding slots, or a second host reporting under its own
  `<host>/<slot>` ids; the heartbeat contract already allows it, so that change
  would not supersede this record.
