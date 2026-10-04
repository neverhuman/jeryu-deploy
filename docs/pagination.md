# Pagination

One rule for both edges: **an out-of-range paging parameter is a `422`, never a
clamp.** A caller that asks for 500 rows and silently gets 100 cannot tell a
short page from the end of the collection, and the walk that follows reads the
same rows twice or skips the rest. The refusal names the field, the range and
says the value was not clamped, so one reply is enough to fix the call.

## `/api/v1`

`limit` (alias `per_page`) is 1 to 500 and defaults to 100; `page` is 1-based.
Sending `limit` and `per_page` with different values is refused: they name the
same thing. Out of range answers `invalid_page_parameter` (422), except
`GET /api/v1/events`, which keeps its own published `events_invalid_query`.

Every paged response carries

- `total` — rows matching the filter **before** paging, and
- a `page` object with the applied `limit` and `page`, that same `total`, and
  `has_more`.

`GET /api/v1/events` is a cursor walk rather than an offset one, so it answers
`limit`, `has_more` and `next_cursor` (the `seq` of its last event): send it as
`after_seq` to keep walking forward, or as `before_seq` on the default
newest-first read. `has_more` is read one row past the page, so a page exactly
`limit` long is not mistaken for "more behind it".

Paged routes: `/api/v1/repos`, `/api/v1/repos/:id/pulls`,
`/api/v1/repos/:id/commits`, `/api/v1/shift/todos`, `/api/v1/audit`,
`/api/v1/releases`, `/api/v1/mirrors`, `/api/v1/settings`,
`/api/v1/agent-runs`, `/api/v1/repos/:id/agent-runs`,
`/api/v1/control-plane/status`, `/api/v1/merge-queue`,
`/api/v1/repos/:id/merge-queue`, `/api/v1/attention` and `/api/v1/events`.
`/api/v1/attention` keeps its `counts` over every item the filter kept, not
over the page, so a page-2 reader still sees how much is waiting.
`/api/v1/control-plane/status` carries many collections at once, so its totals
live in `page.collections.<name>` rather than at the top level.

## `/api/v3`

The GitHub-compatible edge keeps GitHub's `per_page=30` default and `per_page`
ceiling of 100, and pages with the RFC 5988 `Link` header. `per_page` outside 1
to 100, or a `page` below 1, answers GitHub's 422 `Validation Failed` with a
`Pagination` error entry — the same shape the list-query refusals in
`crates/jeryu-api/src/github/listing.rs` use.

`Link` URLs are the URL a client can follow verbatim: the `/api/v3` prefix is
on, the public origin (`JERYU_PRODUCTION_ORIGIN`) in front of it where one is
configured, and the caller's own filters (`?state=closed`, `?sha=`) kept so a
`next` hop stays in the order and filter the first page was read with. `rel`
relations are `next`/`last` while pages remain and `prev`/`first` once past
page 1; a single-page result carries no `Link` at all, as on GitHub. A `?page=`
past the end still answers an empty page, and its `prev` points at the last
page that holds rows rather than at another empty one.

Implementation: `crates/jeryu-api/src/web/paging.rs` (v1) and
`crates/jeryu-api/src/github/support.rs` (v3). The table-driven proof is
`crates/jeryu-api/src/web/paging_tests.rs` for the `/api/v1` collections and
`crates/jeryu-api/tests/github_api.rs` for the v3 list routes; a route added
here belongs in those tables, which is what keeps the rule from holding on some
routes only.
