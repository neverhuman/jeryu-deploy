# Docs Agent Guidance

Owns:
- Architecture (`docs/architecture.md`), testing (`docs/testing.md`), error
  repair (`docs/errors.md`), boundary (`docs/boundaries.md`), generated-zone
  (`docs/generated-zones.md`), audit (`docs/audit-rubric.md`), and
  release-control (`docs/release.md`, a pointer to `scripts/release/README.md`) documentation,
  all routed from root `AGENTS.md`.
- Architecture decision records (`docs/adr/`): the convention in
  `docs/adr/README.md`, the cross-cutting family decisions numbered under it,
  and keeping `Supersedes:`/`Superseded-by:` consistent in both directions.
- Keeping root `AGENTS.md` and `README.md` routed to the same canonical docs.
- Workcell export-slice documentation in `docs/workcell.md`, including the
  release and testing proof commands for typed no-PR denial evidence.
- Workcell run-agent documentation in `docs/workcell.md`, including the route
  proof command for typed path denial and structured event evidence.
- Agent-run control documentation in `docs/workcell.md`, including typed control
  denials, live PTY controls, failed-CI workcell source rules, and proof lane.
- Codegraph oracle route/tool documentation, including schema-v3 MCP/API proof
  commands.
- Codegraph tool-build insight documentation, including fast cluster polling,
  feedback suppression, and MCP/API/CLI proof commands.

Forbidden:
- External hosted-provider authority (including GitHub) or retired
  review-request terminology; the protected `git.neverhuman.org` repository is
  the canonical source and ref authority.
- Aspirational release claims without executable gate evidence.
- Generated artifact edits outside `agent/generated-zones.toml`.

Proof lane:
- `just check` plus `bash ops/ci/workflow-lint.sh`
- `cargo test -p jeryu-api --features web --jobs 40 workcell_run_agent`
  when workcell run-agent route docs change.
- `cargo test -p jeryu-api --features web --jobs 40 agent_runs`
  when agent-run control route docs change.
- `bash ops/ci/codegraph-oracle.sh`
  when codegraph oracle API/MCP docs change.
- `cargo test -p jeryu-api --features web --jobs 40 workcell_export_slice`
  when workcell export-slice docs change.
- `cargo test -p jeryu-api --features web --jobs 40 --test adr_records`
  when `docs/adr/` changes.
- `bash ci-fast-push.sh --no-push` before release-facing docs are signed.
