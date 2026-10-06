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

## Contributions that arrive on a mirror

The mirrors are read-only copies, and the owner-run GitHub ruleset lets only
the mirror identity update a branch there, so nothing can be merged on GitHub.
An outside contributor still opens a pull request there, and an importer brings
it to the forge, where it is gated, reviewed and merged; the next mirror push
carries it back.

Such a pull request is marked, by the importer and only by the importer:

```
PUT /api/v1/repos/{id}/pulls/{n}/origin   {"origin": "contributed"}
GET /api/v1/repos/{id}/pulls/{n}/origin
```

- `JERYU_PULL_IMPORTERS` names the logins that may write the marking, besides a
  global admin. A pull request's own author may never write it, so a
  contribution cannot declare itself trusted.
- A pull request with no marking reads as `internal`, which is what everything
  written inside the forge is.
- Every pull request summary carries `origin:internal` or `origin:contributed`
  in `labels`, so a client that already has the row — the list, the pull
  request page, an agent reviewer — reads the marking without a second call.

A contributed pull request merges only after a login in
`JERYU_PULL_MAINTAINERS` has an effective approval on the exact head. Until
then its merge passport carries `passport_blocked_contributed_origin`, so the
pull request page states the hold, `POST .../merge` refuses it, and the merge
queue will not land it. An approval from anybody else — including an agent
reviewer — is advice and clears nothing. With no maintainer list configured, no
contribution merges at all.

The pull request's title, description and diff are the contributor's words:
they are data everywhere they are read, and nothing in them changes the
marking, the maintainer list, or what the gate decides.

## Importing a mirror's pull requests

The marking above says what a contribution *is*; the importer is what brings
one here. It is off by default and must stay off until the forge can run a
stranger's code safely — see "Before switching it on".

A repository imports when its `[[repo]]` entry carries the mirror fields AND:

```toml
import_github_pulls = true
```

and the forge is armed for the whole instance with
`JERYU_GITHUB_PULL_IMPORT=1`. Either one missing means nothing is imported and
no GitHub credential is even loaded. The token for the API calls comes from
`JERYU_GITHUB_API_TOKEN` or `JERYU_GITHUB_API_TOKEN_FILE`, and never reaches a
process argument: curl reads it from a config on stdin.

For each pull request open on an enrolled mirror the importer:

1. fetches its commits (`refs/pull/<number>/head`, which GitHub publishes for a
   fork and a branch alike) into the forge repository as
   `contrib/gh-<number>`. The commits are fetched, never rewritten, so every
   original author and committer survives;
2. opens a forge pull request from that branch against the mirror's branch,
   titled as the contributor titled it, linking the GitHub pull request, and
   authored by `gh-<github login>` so the importer is never the author of what
   it marks;
3. marks it `contributed`, which is the hold described above;
4. posts the forge gate's answer back as the `jeryu/forge-gate` commit status
   on the GitHub head, and the maintainer review summary as a comment — each
   only when it has something new to say;
5. closes the GitHub pull request once the forge settles it: `merged as <sha>`
   after the mirror push has carried the commits to the branch (GitHub marks a
   pull request merged itself when its exact commits land), or the reason when
   the forge closed it unmerged.

A contributor who force-pushes moves the forge pull request's head with them.
A pull request on a repository nobody enrolled is ignored, and Dependabot is
not a special case: its pull requests take exactly this path.

Deliveries arrive two ways:

- `POST /api/v1/github-mirror/pull-events`, called by the site's webhook relay
  as a `JERYU_PULL_IMPORTERS` identity (or a global admin). The payload is
  untrusted: only the repository name and the pull request number are read, and
  the pull request itself is then read from the GitHub API, so a forged
  delivery cannot hand the forge a head commit of its own.
- the poll, every `JERYU_GITHUB_PULL_IMPORT_MINUTES` (default 5, `0` turns it
  off), which is the fallback for a delivery that never arrived.

A contributor's title, body, branch name and login are data everywhere: they
are copied into the forge pull request and never interpreted, and nothing in
them chooses a repository, a branch or a command.

### Before switching it on

An imported contribution is a stranger's code that the forge's gate then runs.
`import_github_pulls` may not be set, and the forge may not be armed, before
all of these are in place:

- the untrusted-pull-request flag and the sandboxed gate, so a contribution's
  gate run reaches neither the forge nor the network, and
- a live `pr-redteam` running the `jeryu-ci-runner` main code: the older copy
  approves by default and would wave a contribution through.

The contributed-origin hold stops an imported pull request from *merging*
without a maintainer. It does not stop the gate from *running* the code, which
is what the two above are for.

## Turning it off

`JERYU_GITHUB_PUSH=0` loads the mirror with no targets. Merges, tag pushes and
the reconcile all find no target and do nothing, so the forge runs with no
route to GitHub at all.
