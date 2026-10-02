# Product search

`GET /api/v1/search?q=<text>` is the one endpoint that looks across the
product's records, so an operator can find a piece of work without remembering
which page lists it and an agent has something to ask. `/search?q=` in the web
app is this endpoint with an address; the command palette hands its typed text
to that page.

JSON is snake_case. Refusals are the standard envelope
(`code`, `message`, …); see `docs/errors.md`.

Implementation: `crates/jeryu-api/src/web/search.rs`.

## What is searchable

| `kind` | Matched on | Opens |
|---|---|---|
| `repository` | name, `owner/name`, description | `/repos/jeryu/{owner}/{name}` |
| `pull_request` | title, head branch, number, body | `/repos/jeryu/{owner}/{name}/pulls/{n}` |
| `issue` | title, labels, number, body | `/repos/jeryu/{owner}/{name}/issues#{n}` |
| `todo` | title, id, body, note | `/work?family={family}&todo={id}` |
| `activity` | summary, kind, actor, repo, todo id, reason | `/activity?…` |

Each of these is a record the forge already keeps and can read in full from its
own stores: the forge database, the todoq family queues, the pipeline event
log.

**Commit messages and file contents are deliberately not searchable.** Both
mean walking the history or the tree of every repository on every query, which
is an index, not a query, and an index is its own change (retention, staleness,
invalidation on push). Rather than pretend, the answer names the kinds it
searched in `kinds`, so a caller never has to read a silent miss as "no such
thing". Code inside one repository is already findable: the repository page's
file finder, and `POST /api/v1/repos/{id}/codegraph/query`.

## Access

| Kind | Who |
|---|---|
| `repository`, `pull_request`, `issue` | any logged-in account, narrowed to the repositories it may read |
| `todo` | any logged-in account |
| `activity` | global admins |

Search is behind a login even though a public repository is anonymously
readable: one query reads across every repository at once, and the shape of the
answer would leak which private names exist. Repository, pull request and issue
hits are filtered by the same rule `GET /api/v1/repos` applies, so search can
never name something its list would hide. `activity` is admin-only for the same
reason `GET /api/v1/events` is: events carry todo titles, notes and log tails
from repositories the reader may not see.

A non-admin who asks for `kind=activity` is refused (`403`). A non-admin who
asks for no kind in particular gets the four kinds they may read, and `kinds`
says so.

## Request

| Parameter | Meaning |
|---|---|
| `q` | the text to find. Required, non-empty, at most 200 characters. |
| `kind` | comma-separated kinds to search. Default: every kind the reader may search. |
| `limit` | hits returned **per kind**, 1–100. Default 10. |

`q` also understands the reference spellings a person types: `12`, `#12`,
`name#12` and `owner/name#12` find that pull request or issue by number, and a
qualified `name#12` looks only in the repository it names.

## Response

```json
{
  "generated_at": "2026-09-29T09:00:00Z",
  "query": "receipt",
  "kinds": ["repository", "pull_request", "issue", "todo", "activity"],
  "counts": { "issue": 2, "pull_request": 2, "repository": 0, "todo": 1 },
  "limit": 10,
  "results": [
    {
      "kind": "pull_request",
      "id": "pull_request:acme/gatekeeper#2",
      "title": "#2 Write the receipt after the gate",
      "context": "acme/gatekeeper · open",
      "path": "/repos/jeryu/acme/gatekeeper/pulls/2",
      "updated_at": "2026-09-29T08:58:11Z",
      "repo": { "id": "…", "host": "jeryu", "owner": "acme", "name": "gatekeeper" }
    }
  ],
  "problems": []
}
```

- `counts` is the number of matches per kind **before** `limit` cut the list,
  so a page can say "7 repositories" while showing three.
- `snippet` is the matching line when what matched was a body rather than a
  name, capped at 200 characters. A repository hit falls back to its
  description, which is the one line worth showing either way.
- `problems` names sources that could not be read on this query (a family queue
  whose repository is unreadable, say). Empty is the normal case; a broken
  source narrows the answer instead of failing the whole search.

## Ranking

Results are grouped by kind in the order of the table above, and within a kind
ordered by where the query matched: the exact name, then a name that starts
with it, then a word of the name that starts with it, then a name that contains
it, and last a body that contains it. Ties break by recency, then title. A
repository called `forge` therefore always outranks every description that
mentions forges.
