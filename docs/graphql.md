# GraphQL on this forge

The forge answers `POST /graphql`, `POST /api/graphql`, and `POST /api/v3/graphql` identically, for
a small read-only subset of GitHub's GraphQL API: discovery probes and one
captured GitHub CLI PR read. Anything else is refused with a `501` that
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

Scoped agent tokens (`jat_`) may run the bounded PR query below. Repository
reach is checked by the same authorization service as the REST PR read.
As with REST, task-scoped credentials may read throughout their task's repository. Other GraphQL queries return `403 insufficient_scope` for scoped tokens.
Account repository reads honor the authenticated principal; unreadable
repositories appear as `null`.

## Tested GitHub CLI command

GitHub CLI **2.45.0** is tested with this exact command:

```sh
gh pr view <number> --repo <forge>/<owner>/<repo> --json number,title,state,isDraft,headRefOid,url
```

It sends one `PullRequestByNumber` query to `/api/graphql`, adding `id` to the
selection internally. Only the captured selection and variable types are
accepted (whitespace may vary). Extra fields, aliases, other selections and
mutations receive guided errors. PR state is `OPEN`, `CLOSED` or `MERGED`;
`isDraft` and `headRefOid` come from the stored PR. URLs use the configured
public origin, as REST PR URLs do.

The required API test `actual_gh_245_pr_view_reads_four_lifecycle_fixtures`
runs the real binary through an ephemeral loopback TLS recorder into the
authenticated Axum router, checking draft, ready, closed and merged data and
the captured request in `crates/jeryu-api/tests/fixtures/gh-pr-view/request.json`.
It requires `gh` 2.45.0, Python 3 and OpenSSL on PATH; missing or different tools
fail the fixture. Run it with:

```sh
cargo test --offline -p jeryu-api --features web gh_pr_view
```

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
