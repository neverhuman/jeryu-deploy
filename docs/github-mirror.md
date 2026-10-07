# The GitHub mirror

The forge is the truth and `github.com/<org>/*` is an exact copy of it.
This page says how a family enrols, what the mirror does on its own, and what
it refuses to do.

The rule behind every decision here is [ADR
0001](adr/0001-forge-is-origin-github-is-a-downstream-mirror.md): the flow is
one-way. The mirror reads GitHub to report on it, never to adopt it.

## Enrolling a family

A repository is mirrored when its split-manifest `[[repo]]` entry carries all
three of:

```toml
[[repo]]
jeryu_slug = "jeryu/jeryu-core"          # owner/name on the forge
github_slug = "acme-oss/jeryu-core"      # owner/name on GitHub
default_branch = "main"                  # optional; defaults to main
mirror_github_main = true
```

A family enrols by adding those fields to every repository in its manifest and
handing the manifest to the server with `--split-manifest`. There is nothing
per-family to switch on: the server loads every manifest it is given, and a
repository without all three fields is simply not a target. Entries with
`mirror_github_main = false` are ignored, which is how a repository opts out
while staying in the manifest.

Two things are needed on the forge host once, for the whole forge:

- the git `url."git@github-mirror:".insteadOf` rewrite in the global config of
  the user the forge runs as, and
- the matching SSH deploy key with push rights on the GitHub side.

Nothing secret lives in the manifest: the destination is built as
`https://x-access-token:jeryussh@github.com/<github_slug>.git` and the rewrite
turns it into the SSH push. The token in that URL is a fixed public stand-in,
and anything git echoes is redacted before it reaches a check-run or the
inbox. When the rewrite or the key is missing, every push fails and the
attention inbox raises `mirror_failing` with the command to check.

## Which remote may be reached

A `github_slug` from a manifest ends up inside a URL and then on a git command
line, so it is parsed and allowlisted before any target resolves
(`crates/jeryu-api/src/git_remote.rs`). `JERYU_MIRROR_REMOTE_ALLOWLIST` lists
the `host/owner` pairs the mirror may reach, separated by whitespace or commas:

```
JERYU_MIRROR_REMOTE_ALLOWLIST='github.com/acme-oss'
```

Which GitHub owner this forge mirrors into is a site value, so it has no default
here: with the variable unset, every push, tag pass and reconcile reports that
it is not configured and reaches nothing. A slug whose URL is not an `https`
URL whose host and first path segment match an entry exactly is refused the same
way, as is one carrying userinfo, an explicit port, a `.` or `..` or
percent-encoded path segment, a control character, or a leading dash. The push
credential is attached only after the credential-free URL has been allowed.

## What the mirror does

| Trigger | What it pushes |
|---|---|
| A pull request merges into the default branch | the live branch tip, fast-forward only |
| A tag push | each tag GitHub does not have yet |
| The reconcile, every 10 minutes | the branch when GitHub is behind, plus any tag GitHub is missing |

Everything is fast-forward. `--force` appears nowhere, and no code path deletes
a ref on GitHub.

## The reconcile

`crates/jeryu-api/src/web/mirror_reconcile.rs` walks the enrolled repositories
on a timer, off the request path, one repository at a time, with the mirror's
bounded git calls. `JERYU_GITHUB_MIRROR_RECONCILE_MINUTES` sets the cadence
(default 10; `0` turns it off). For each repository it compares the forge's
branch and tags with what GitHub advertises and lands in one of these states:

| State | Meaning | What happens |
|---|---|---|
| `in_sync` | GitHub main is the forge main | nothing |
| `behind` | GitHub is missing forge commits | fast-forwarded; `behind` is only reported when the push itself failed |
| `ahead` | GitHub holds every forge commit and more — somebody merged on GitHub | nothing is pushed; alarm |
| `diverged` | both sides hold commits the other does not | nothing is pushed; alarm |
| `unknown` | GitHub could not be read (network, auth, no key on this host) | nothing is pushed; the error is shown |

A tag GitHub publishes at another commit, or a tag GitHub has and the forge
does not, is drift: it is reported with both oids and never resolved by the
mirror, because only a person knows what was cut from that tag downstream.

## Seeing it

- **Repo page.** `GET /api/v1/mirrors[?repo=]` carries `mirror` (the push
  bookkeeping: last attempt, whether it worked, last success) and `sync` (what
  the last reconcile saw: state, forge head, GitHub head, when a push last put
  them level, GitHub-only commits, tag drift).
- **Checks.** A push records `jeryu/github-mirror`. A state only a person can
  settle records `jeryu/github-mirror-divergence`, written when the posture
  changes rather than on every pass, so a ten-minute loop does not grow the
  check-run history.
- **Needs you.** `mirror_failing` (a push is failing, one item for the forge)
  and `mirror_diverged` (GitHub holds work the forge does not — one item per
  repository, naming the commits). See `docs/pipeline-events.md`.

## Turning it off

`JERYU_GITHUB_PUSH=0` loads the mirror with no targets. Merges, tag pushes and
the reconcile all find no target and do nothing, so the forge runs with no
route to GitHub at all.
