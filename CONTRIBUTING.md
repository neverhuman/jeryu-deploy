# Contributing

Outside pull requests are welcome on GitHub. The GitHub repository is a
read-only mirror: the forge is where a pull request is gated, reviewed and
merged. When that happens the GitHub pull request is closed with a link to the
merged change.

**Status:** the importer that brings GitHub pull requests to the forge is not
live yet. Until it is, pull requests are read but not merged. See
`docs/github-mirror.md` for how the mirror works.

## Tests and fixtures

Use invented data only: organisations such as `acme` and `globex`, hosts under
`example`. Never put real hosts, logins, paths or credentials in code, tests,
fixtures or docs. Site values come from configuration.

## Run the gate locally

```sh
just required
```

Run it before you open a pull request and fix what it reports.

## Sign-off

Commits of a contributed head currently need a `Signed-off-by` line matching
the author (`git commit -s`); the forge checks this when it imports the
pull request. No CLA is required.
