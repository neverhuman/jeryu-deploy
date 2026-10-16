# 0015. A native repository's browser URL omits the provider

Status: Accepted
Date: 2026-10-16
Supersedes: none
Superseded-by: none

## Context

The forge's own pages are served under a provider-qualified route:
`/repos/{provider}/{owner}/{repo}` and `/repos/{provider}/{owner}/{repo}/pulls/{n}`.
The provider segment exists because a repository can be mirrored from another
host, and two hosts can carry the same `owner/name`.

For the repositories this forge hosts the segment is always `jeryu`, and it is
the part of the URL nobody remembers. An agent that shortened a pull request
link to `/repos/{owner}/{repo}/pulls/{n}` shared a URL that was not a page at
all; worse, that path answers valid JSON on the compatibility REST edge, so the
HTTP 200 it checked looked like proof.

The native API made that easy to get wrong: `GET /api/v1/repos/{id}/pulls/{n}`
returned no browser link of any kind, so a client had one option — build a
route from parts.

## Decision

A repository this forge hosts has one canonical browser URL, and it omits the
provider:

- repository: `/{owner}/{repo}`
- pull request: `/{owner}/{repo}/pull/{number}`, with its tab appended
  (`/files`, `/checks`, `/commits`)

A repository mirrored from another host keeps `/repos/{provider}/{owner}/{repo}`:
guessing between two hosts that carry the same `owner/name` would open a
repository nobody asked for, and where neither is ours the web app asks which
one rather than choosing.

`crates/jeryu-api/src/links.rs` is the one policy, server-side, and
`apps/web/src/pages/canonicalLinks.ts` is the same policy in the web app. Both
read the public origin from configuration (`JERYU_PRODUCTION_ORIGIN`); no
deployment hardcodes a domain.

Native repository and pull request responses — detail and list rows — carry
two separate fields: `html_url`, the canonical browser URL, and `api_url`,
where the same object is fetched as JSON. Clients are told, in
`GET /api/v1/capabilities` and in the agent documentation, to share the
returned `html_url` and never to assemble a route.

This extends [0002](0002-html-url-is-jeryu-shaped.md) rather than replacing it:
`html_url` still targets this forge and never its downstream mirror. The
GitHub-compatible edge keeps the provider-qualified `html_url` its clients
already read, and provider-qualified browser URLs keep resolving to the same
page, so nothing in circulation breaks.

## Consequences

- A link from the native API is short enough to retype and unambiguous enough
  to be right, and a reader who follows it lands on the object it names.
- Two shapes address one object. The provider-qualified one is what the web
  app's own pages and in-app links use, so a click inside the app takes no
  hop; the short one resolves onto it. Neither redirects to the other in a
  cycle, and tests hold that.
- The short shape occupies the top-level namespace, so a new reserved
  top-level route (`/work`, `/flow`, …) is a name no owner can have. The web
  app declares every reserved route before the `:owner/:repo` pattern, and a
  test keeps that order.
- The two link fields are added when a response is serialized, beside the
  shared read-model types rather than inside them: `RepositorySummary` and the
  pull request contracts live in `jeryu-core`, which this repository consumes
  through a split tag, so a field in them cannot be used in the same change
  that adds it. Carrying `html_url` and `api_url` in the generated contract is
  a separate step: change `jeryu-core`, cut its tag, then bump the pin here and
  in `jeryu-web` and drop the hand-written declarations. The wire shape does
  not change when that happens.
- Two policy implementations, one per repository, have to agree. They are both
  small, both tested, and the API-returned `html_url` wins wherever a payload
  carries one — the forge knows its public origin, the app only knows the
  origin it was loaded from.
