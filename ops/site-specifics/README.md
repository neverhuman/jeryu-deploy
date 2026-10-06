# Site-specifics scanner

The jeryu repositories are public and must be usable by anyone, so they must
not name one installation. Hosts, user names, absolute home paths, private
address ranges and credential file names belong in configuration outside the
repository: an environment variable with no site default, or a file under
`~/.config/jeryu/`. Tests, fixtures and docs use invented data instead
(`acme`, `globex`, `node-a`).

`scan.sh` finds the places that still name a site.

## The two files the site keeps

Both live outside this repository, because the terms are themselves the thing
that must not be published:

- `JERYU_SITE_TERMS_FILE` (required) — one term per line, a POSIX extended
  regex, matched case-insensitively. `#` starts a comment. Without it the
  scanner refuses to run.
- `JERYU_SITE_ALLOW_FILE` (optional) — matches it covers do not fail `--ci`.
  One entry per line, either `<path-glob>` or `<path-glob>:<term>`, where the
  term is the term exactly as written in the terms file.

A site's terms file looks like this, with its own values in place of the
invented ones:

```
# hosts
node-[a-z]
# accounts and home paths
acme-admin
/home/acme
# private range
10\.0\.0\.
```

## Running it

```sh
export JERYU_SITE_TERMS_FILE=~/.config/jeryu/site-terms

ops/site-specifics/scan.sh                      # this checkout
ops/site-specifics/scan.sh ../jeryu-web ../jeryu-core
ops/site-specifics/scan.sh --quiet ..           # counts only
JERYU_SITE_ALLOW_FILE=~/.config/jeryu/site-allow \
  ops/site-specifics/scan.sh --ci               # CI mode: fails on a match
```

## What legitimately stays

One file in the family is a site's own declaration rather than product source:
`jeryu-release-ops/repos.manifest.toml`, the family authority, records where
that installation keeps its checkouts. Consumers read those paths, so they
cannot move into an environment variable; the validator that reads the manifest
derives every path from the root the manifest itself declares, and carries no
site value of its own. Put that one file in `JERYU_SITE_ALLOW_FILE`:

```
repos.manifest.toml
```

Everything else is either a required setting with no default, or invented data.

Each match prints as `file:line:term` — never the matched line, which can
itself be site data — followed by a count per path scanned. `--ci` exits 1 when
a match is not covered by the allowlist, 2 on a usage or configuration error.

Only tracked files are scanned, since only they are published; a directory that
is not a git checkout falls back to every file under it.

## Tests

`ops/site-specifics/test-scan.sh` covers the refusal, the matching, the counts
and both allowlist forms. It uses invented terms, so no site data is in this
repository, including its tests.
