# Codegraph Oracle

`jeryu-codegraph` is Jeryu's hosted code oracle for repo/ref impact context.
It indexes a materialized repository checkout at a resolved commit, stores the
refresh in its own SQLite database, and returns a provenance-bearing
`CodeGraphImpactPack`.

## Contract

- REST: `POST /api/v1/repos/{id}/codegraph/query`
- MCP: `jeryu.codegraph.query`
- CLI: `jeryu-codegraph query --root . --changed <path> --json`

The request accepts a ref, repo-relative changed paths, optional intent/question
text, and an optional token budget. The response includes the resolved commit,
changed and affected Rust crates, affected public symbols, must-read files,
should-read files, proof lanes, suggested commands, excluded heuristic matches,
graph stats, residual risk, provenance, and an index receipt.

## Where the index lives, and what fills it

Each repository has its own index inside its bare repo, at
`<git storage>/<owner>/<name>.git/jeryu/codegraph.sqlite`. The symbol,
reference and crate-dependency tables carry no repository column, so one
database per repository is what makes the `repo` argument of the `code.*` MCP
tools scope results to the repository the caller named. The server's
`codegraph.sqlite` under its data directory stays the cross-repo store the
tool-build cluster reads use.

Three things fill a repository's index, each writing the whole snapshot in one
transaction (an interrupted refresh leaves the previous index in place):

- a push that moves a repository's default branch, refreshed after the refs are
  durable so indexing never holds the push acknowledgement;
- the refresh timer, which every ten minutes refreshes any repository whose
  index is empty or older than `JERYU_CODEGRAPH_REFRESH_INTERVAL_HOURS`
  (default 6; `0` disables the timer, not the push refresh);
- a `code.*` read for a repository with no symbols yet, which indexes its
  default branch once and answers from the result.

A repository with no Cargo workspace, or with no commit on its default branch,
is reported on the server log and skipped; v1 indexes Rust/Cargo only.

What an index holds is what the v1 analyzer produces: public symbols per crate,
crate dependency edges, and file rows with governance metadata. It records no
symbol reference rows, so `code.references` answers from an indexed repository
with an empty list until the analyzer collects them.

## V1 Scope

Rust/Cargo graph reachability is the authoritative analyzer in v1. Governance
metadata is ingested from these files when present:

- `AGENTS.md`
- `agent/owner-map.json`
- `agent/test-map.json`
- `agent/generated-zones.toml`
- `agent/proof-lanes.toml`

Generated-zone and owner/proof-lane metadata stays attached to impacted files so
agents can see editability and proof obligations. Lexical intent/question
matches are reported only in `excluded_files`; they are never promoted to
`must_read_files`.

## Proof

- `cargo test -p jeryu-codegraph --jobs 40`
- `cargo test -p jeryu-api --features web --jobs 40 codegraph`
- `cargo test -p jeryu-mcp --jobs 40`
- `jankurai diff-audit --base-ref origin/main .`
