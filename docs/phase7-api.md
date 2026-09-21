# AgentBridge Typed API

This archive implements AgentBridge as an in-process Rust API. The method names map directly to the Phase 7 HTTP surface:

| HTTP target | Rust method |
| --- | --- |
| `GET /api/agent/context?repo=&pr=` | `AgentBridge::context` |
| `GET /api/agent/mergeability?pr=` | `AgentBridge::mergeability` |
| `POST /api/agent/dry-run/patch` | `AgentBridge::dry_run_patch` |
| `POST /api/agent/proof-plan` | `AgentBridge::proof_plan` |
| `POST /api/agent/run-proof` | `AgentBridge::run_proof` |
| `POST /api/agent/propose-fix` | `AgentBridge::propose_fix` |
| `POST /api/agent/hotfix` | `AgentBridge::hotfix` |
| `GET /api/agent/receipts/{id}` | `AgentBridge::receipt` |

The type layer enforces SHA binding, path scopes, receipts, and proof witnesses.

# `/api/v1` wire conventions

New and changed `/api/v1` serialisers follow these rules. Existing routes that
still break one are listed under *Known deviations* and converge one route
family per change, together with the jeryu-web caller that reads them.

## Naming

- JSON keys are `snake_case`. `#[serde(rename_all = "camelCase")]` is not used
  on `/api/v1` types; one document never mixes the two cases.
- Enum values are `snake_case` strings (`"in_progress"`, not `"InProgress"`).
- Booleans read as predicates: `has_more`, `is_default`, `enabled`.

## Ids

- Every resource has exactly one `id`: an opaque string, stable for the life
  of the resource, and the value used in its detail URL.
- A reference to another resource is `<resource>_id` (`repo_id`, `run_id`).
  Human handles (`owner/name`, PR `number`, slug) may appear beside the id but
  never replace it, and never under the key `id`.
- Numeric ids are serialised as strings.

## Timestamps

- Every timestamp is an RFC 3339 string in UTC with a `Z` suffix
  (`2026-09-21T09:03:00Z`, optional fractional seconds). Never unix seconds,
  never unix millis, never a millis number carried as a string.
- Keys end in `_at` (`created_at`, `updated_at`, `scanned_at`,
  `generated_at`). Durations end in `_ms` and are integers.
- A time that is not known is omitted or `null`, never `0` or `""`.

## List envelopes

A collection returns one shape:

```json
{ "items": [ ... ], "page": { "limit": 100, "page": 1, "total": 312, "has_more": true } }
```

- The array is always `items`, never `data`, `results`, or a resource-named
  key; a bare top-level array is not returned.
- `page` is `paging::PageInfo` (see `crates/jeryu-api/src/web/paging.rs`);
  unpaged collections may omit it.
- A list row carries the same keys, with the same meanings, as the detail
  document for that resource. Detail may add fields; it may not rename or
  re-type ones the row has.

## Request ids and versions

- `request_id` is the per-request correlation id chosen by the
  `web/request_id.rs` middleware and returned in the `x-request-id` header;
  a body that echoes it uses the same value. No other field is called `request_id`; a caller-supplied
  idempotency key is `idempotency_key`.
- `api_version` is the wire contract version (`"v1"`, matching the path).
  `server_version` is the running build. A resource's optimistic-concurrency
  counter is `revision`. The bare key `version` is not used.

## Status codes and request handling

One rule per class, applied in front of the handlers rather than per route:

- Input that cannot be read or fails validation (malformed JSON, a body of the
  wrong shape, a bad query string, a path segment of the wrong type) answers
  **422** with the error envelope. A 400 from any layer is rewritten to 422.
- A well-formed id that names nothing answers **404**; an unknown route answers
  404 `api_route_not_found`.
- A trailing slash names the same route: `/api/v1/work/` is `/api/v1/work`.
- An `Accept` header that admits no JSON (`*/*`, `application/*`,
  `application/json`, `*+json`) answers **406** `not_acceptable`. Writes are
  refused before they run; reads that stream text (raw files, logs, SSE) still
  answer their own media type.
- `OPTIONS` is a CORS preflight and answers **204** without a login. No origin
  is granted, so cross-origin browser calls stay blocked.

## Known deviations

- `control_plane`, `auth`, `ecosystem` and `ci_evidence` types still serialise
  `camelCase` keys for the jeryu-web views that read them; they move to
  `snake_case` with a matching jeryu-web change.
- Collections that still use `data`/`results`/resource-named arrays migrate
  to `items` + `page` route family by route family.

Converged: `GET /api/v1/tool-finder/dashboard` reports `scan.scanned_at` as
RFC 3339 UTC (it was the persisted epoch-millis string).
