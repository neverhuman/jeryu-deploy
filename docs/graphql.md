# GraphQL on this forge

The forge answers `POST /graphql`, and `POST /api/v3/graphql` identically, for
a small read-only subset of GitHub's GraphQL API — the discovery probes GitHub
clients open a connection with. Anything else is refused with a `501` that
names the REST route serving the same intent. The forge never executes
arbitrary GraphQL.

The subset is published, not guessed at: `GET /api/v1/capabilities` carries it
under `graphql_support.supported_operations`, `GET /api/v1/docs/rest` under
`graphql.supported_operations`, and the `501` body as
`graphql_supported_operations`. All four come from one list in the server, so
they cannot disagree with what it answers.

## What is served

```graphql
query { __typename }
query { viewer { login } }
query {
  repository(owner: "<owner>", name: "<name>") {
    name
    nameWithOwner
    isPrivate
    defaultBranchRef { name }
    url
  }
}
```

The arguments of `repository` may be inline strings or GraphQL variables
(`owner`, and `name` or `repo`).

## Authentication

Authenticate as an account: a web session, or a personal access token
(`jpat_`) as `Authorization: Bearer <token>`.

A **scoped agent token (`jat_`) is refused here** with `403 insufficient_scope`.
Agents use the GitHub-compatible REST subset or `/mcp`, which is where their
scoping is enforced; `connect-an-agent.md` says the same. `viewer` is the
authenticated account, and a repository that account may not read reads as
`null` — GraphQL's shape for "no such node", which tells a caller nothing about
whether the repository exists.

## What is not served, and what to send instead

Everything else, including every mutation. A refusal carries
`jeryu_repair_hint` and `jeryu_api_routes`: the GitHub-compatible REST routes
for repositories, pull requests, issues, commits, checks, releases and hooks.
For example:

| GraphQL you wanted | Send instead |
|---|---|
| `repository { pullRequests { ... } }` | `GET /repos/{owner}/{repo}/pulls` |
| `repository { issues { ... } }` | `GET /repos/{owner}/{repo}/issues` |
| `repository { object/ref { ... } }` | `GET /repos/{owner}/{repo}/commits` |
| `mutation { createPullRequest }` | `POST /repos/{owner}/{repo}/pulls` |
| `mutation { mergePullRequest }` | `PUT /repos/{owner}/{repo}/pulls/{number}/merge` |
| `mutation { addComment }` | `POST /repos/{owner}/{repo}/issues/{number}/comments` |

Widening the subset means adding a matcher, a conformance test in
`crates/jeryu-api/tests/github_api.rs`, and the operation to the published
list — in that order.
