# Architecture

Jeryu is a local GitHub-compatible forge implemented as Rust workspace crates and local operational scripts. Compatibility means matching observable API and workflow behavior where that is useful for users and agents; it does not mean copying GitHub source, bundling GitHub assets, or requiring a hosted GitHub dependency.

This checkout owns source only for `crates/jeryu-api`, `crates/jeryu-cli`, and
`crates/jeryu-split-tool`. Core, Git storage, CI, runner, cache, proof, agent,
codegraph, and signing packages are immutable Git dependencies owned by their
standalone repositories. Deploy may exercise their public contracts through
its API/CLI graph, but it must not present their source gates as local source.
See `docs/codegraph-oracle.md` for the composed codegraph contract.

The shared workcell control plane is part of the runner/CI stack, not a separate subsystem. `jeryu-runnerd` owns warm-pool claims, epoch-fenced release/heartbeat handling, startup rebase enforcement, and quarantine-first tar validation on top of the existing runner fabric.

Deploy's agent-run edge keeps one wire-type and routing authority in
`web/agent_runs.rs`. Child modules isolate HTTP/MCP/driver orchestration,
bounded run and raw-TTY state, and frozen-diff pull-request export. They expose
only the same `web`-scoped surface as the former single file; persistence,
runner authority, and serialized contracts remain owned by their existing
components.

The API keeps the same module and route authority while isolating split-family
catalog loading, bootstrap credential custody, session runtime preparation,
repository source reads, pull-request merge posture, and installed Jankurai
verification in child modules. The HTTP and MCP edges both apply one bounded
`x-request-id` middleware: portable caller IDs round-trip, while empty, hostile,
or oversized values are replaced before handlers observe them.

The R5 proof lane lives in `crates/jeryu-api` and closes the loop from claim to reviewed pull request: rebase, jailed edit, namespaced branch export, PR creation, and CI evidence verification. The export request carries the changed-file list so the pull request preserves branch ownership and reviewer-visible edit scope.

JMCP/control-plane intelligence is an API/read-model boundary over local truth:
the local forge store, runner fabric, workcells, agent runs, codegraph, and
tool-build clusters are authoritative, while GitHub mirror data is optional
read-only evidence that must degrade as `missing`, `stale`, `queued`, `failed`,
or `unknown` rather than becoming an implicit green signal.

The canonical reproducible validation surfaces are `Justfile`, `ops/ci/*.sh`,
`ops/ci/gates/*.sh`, and `agent/test-map.json`; protected hosted refs and the
exact-head required context are merge authority.

`tools/security-lane.sh` owns the security implementation. Compatibility
callers may enter through `ops/ci/security.sh`, but that file delegates without
reimplementing checks; the comprehensive and pull-request gates enable network
dependency audits explicitly.

## External task-card queues

A hosted `<family>-todo` repository can set `task_cards = true` under
`[family]` in `family.toml` on its `queue` branch. The queue reader then reads
both native `todos/*.md` and existing Markdown `tasks/*.md`. Other families
retain their native TODO behavior. Cards take precedence over native records
with the same stable `<family>-<lowercase-task-id>` ID during migration.

Card filenames carry IDs such as `T001-API-01-contract.md` or
`API-01-F01-integration.md`. The original card body is displayed unchanged;
Markdown table fields supply dependencies, status and external assignment.
The optional existing `index.json` supplies title, priority and dependency
metadata for cards whose layouts differ. Missing or ambiguous cards,
duplicate IDs, invalid metadata, unresolved dependencies, oversized inputs
and symlinks are reported as queue read failures rather than silently hidden.
Reads use blobs at one immutable queue commit and never access live agent
directories, follow symlinks, or run commands from a card.

Primary receipts in `status/<ID>.md` supply review/blocker observations. A
receipt saying merged is a reported handoff, not verified completion. Optional
`tracking/observations.json` records dated external PR evidence: an entry with
`status = "done"` and nonempty, all-merged PRs can mark delivery merged;
partial delivery remains handoff. The date and links remain visible, and
acceptance criteria and owner/qualification gates stay in the original card.
The server never makes external network calls to infer PR state. Push source
and observation updates to refresh what the forge reads. External PRs do not
become native forge PR or release records.

Task cards are externally maintained: TODO actions return
`409 external_task_read_only` before making any commit. Comments remain in
separate `comments/<todo-id>.jsonl` files. Registering this read surface does
not configure workers, supervisors, schedules, or execution authority. Keep
externally managed families outside worker configuration.

Rerun the parser, Git-tree integration and HTTP immutability proofs with:

```sh
cargo test --locked --offline -p jeryu-api --features web --lib task_cards
```

## Standalone Delivery Authority

This split repository composes immutable sibling releases into the API and CLI
delivery graph. It consumes the governed Jankurai identity rendered from the
protected `jeryu-tool` manifest, but it never issues, signs, or silently
substitutes that identity. The release-broker path is a deployment boundary:
release CI must resolve the single physical broker binary before any score or
artifact decision is trusted.
