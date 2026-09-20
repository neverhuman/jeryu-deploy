# 0002. `html_url` is jeryu-shaped, not GitHub-shaped

Status: Accepted
Date: 2026-09-20
Supersedes: none
Superseded-by: none

## Context

The GitHub-compatible edge (`crates/jeryu-api/src/github/`) answers with
GitHub-shaped JSON so that existing clients work unchanged, and the
`github_api` conformance tests hold those shapes byte-for-byte. Every such
resource carries an `html_url`: the address a human is sent to when they click
through from an API object.

Two readings were available. `html_url` could name the GitHub mirror, which
makes the payload identical to GitHub's and lets a client's "open in browser"
land somewhere familiar. Or it could name the jeryu web UI, which is where the
object actually lives.

The mirror is downstream and may lag ([0001](0001-forge-is-origin-github-is-a-downstream-mirror.md)), so a GitHub-shaped `html_url`
can point at a commit that does not exist yet, or at no object at all for
anything the mirror does not carry — pull requests, checks, workflow runs,
every repository without `mirror_github_main`.

## Decision

`html_url` addresses the jeryu web UI. Field names and status codes stay
GitHub-shaped; the URLs inside them are ours.

- Every `html_url` is built through `github::support::web_url`, which prefixes
  a rooted web-UI path with the public origin from `JERYU_PRODUCTION_ORIGIN`
  (e.g. `https://git.neverhuman.org`). Without a configured origin — local
  dev, tests — the path is returned unchanged.
- No resource on the GitHub-compatible edge emits a `github.com` URL.

## Consequences

- A link from the API always resolves to the object it describes, including
  the objects that exist only on the forge.
- A client that parses `html_url` expecting a `github.com` host will not find
  one. That is the intended break: such a client is asserting GitHub is origin,
  which it is not.
- Correct links depend on `JERYU_PRODUCTION_ORIGIN` being set in production.
  Unset, the edge emits relative paths — harmless in dev and tests, wrong in
  production, so the production unit sets it.
- Changing the public origin changes every `html_url` at once, and old absolute
  links captured elsewhere go stale. Link rot is preferred to pointing at a
  copy.
