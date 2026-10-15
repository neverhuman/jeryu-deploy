# Configuring a forge

`jeryu serve` takes a few command-line flags and reads everything else from its
environment. `configs/jeryu.env.example` lists every environment variable the
server reads, grouped by area, each with what it does, whether it is required,
its default and its format. This page says how to load that file, which
settings a working forge needs, and how to check them.

`crates/jeryu-api/tests/operator_env_reference.rs` fails the required gate when
the server reads a variable the example does not list, or the example lists one
the server no longer reads.

## Where

**Flags** (not environment):

| Flag | Default | Meaning |
|---|---|---|
| `--bind` | `127.0.0.1:8787` | listen address; keep it on loopback behind an edge proxy |
| `--data-dir` | `JERYU_DATA_DIR`, else `$XDG_DATA_HOME/jeryu`, else `~/.local/share/jeryu` | repositories, SQLite stores, keys |
| `--split-manifest` | none | a split-family manifest; repeat for several. Enrols repositories in the GitHub mirror and the tool-finder |
| `--spa-dir` | `apps/web/dist` | development only; release builds embed the pinned web UI |

**Environment file.** Copy the example, fill it in, keep it private:

```sh
install -d -m 700 ~/.config/jeryu
install -m 600 configs/jeryu.env.example ~/.config/jeryu/jeryu.env
```

Then load it one of three ways:

- **systemd** (the release unit, `scripts/release/systemd/jeryu.service.in`,
  already has `EnvironmentFile=-%h/.config/jeryu/jeryu.env`). For your own unit:

  ```ini
  [Service]
  EnvironmentFile=/etc/jeryu/jeryu.env
  ExecStart=/usr/local/bin/jeryu serve --bind 127.0.0.1:8787
  ```

  Do not edit the release unit; it is replaced by every release. Extra files
  go in a `jeryu.service.d/` drop-in (`scripts/release/README.md`, "A site's
  own environment and drop-ins").
- **Docker**: `docker run --env-file /etc/jeryu/jeryu.env ... jeryu serve
  --bind 0.0.0.0:8787` (inside a container, loopback is unreachable from the
  host). Docker passes `NAME=` through as an empty variable, which the example
  treats exactly like unset; it skips `#` lines.
- **Shell**: `set -a; . /etc/jeryu/jeryu.env; set +a; jeryu serve`.

The file is plain `NAME=value` lines: no quotes, no `export`, no `$VAR`
expansion (systemd and Docker do not expand them).

## Minimal forge

Nothing is required for a forge on one machine. With an empty file:

1. `jeryu serve` listens on `127.0.0.1:8787` and keeps data in
   `~/.local/share/jeryu`.
2. The first start creates the admin `jeryu-admin` and writes its one-time
   password to `<data_dir>/bootstrap-credentials.json` (mode 600). Sign in,
   set a new password, then delete that file.
3. Mail, sign-up, the GitHub mirror, the importer and every automation
   identity stay off; accounts are made by the admin.

## Production forge

Put an edge proxy (TLS) in front of the loopback listener, then set:

| Area | Settings |
|---|---|
| Storage | `JERYU_DATA_DIR` (for example `/var/lib/jeryu`); back the whole directory up, including `bot-signing.key` |
| Public origin | `JERYU_PRODUCTION_ORIGIN=https://forge.example.com` |
| Edge proxy | `JERYU_TRUSTED_PROXIES` = the address the proxy connects from (`127.0.0.1,::1` for a proxy on the same host). Exact IPs only, no CIDR |
| Mail | `JERYU_MAIL_FROM`, `JERYU_MAIL_BASE_URL`, and one of `JERYU_MAIL_SENDMAIL` / `JERYU_MAIL_SPOOL`; details in [account-email.md](account-email.md) |
| Sign-up | closed unless `JERYU_SIGNUP=email` (needs mail); `JERYU_WAITLIST_URL` = an absolute https URL where people join the waitlist instead ([account-email.md](account-email.md#sign-up)) |
| Automation | `JERYU_AUTOMATION_IDENTITIES`, the shift/reporting and review/merge identities, `JERYU_CI_PUBLISHERS`, and `JERYU_JANKURAI_SCORERS` enforce service kind ([bots.md](bots.md), [pipeline-events.md](pipeline-events.md)) |
| Deployment permission | `JERYU_DEPLOYERS` names admins allowed to record deployments. These accounts default to service; explicitly mark a person `human` through the admin users API. Other configured automation roles still enforce service kind |
| Mirror | `JERYU_MIRROR_REMOTE_ALLOWLIST` plus `--split-manifest` ([github-mirror.md](github-mirror.md)) |

Leave `JERYU_WEB_TRUST_LOCAL` and `JERYU_CI_MOCK` empty on any forge an edge
exposes.

## Check

Every value is read at startup or on first use and then kept, so **a change
takes effect only after a restart** (`systemctl --user restart jeryu.service`).
After restarting:

- `curl -fsS http://127.0.0.1:8787/health` answers.
- The log (`journalctl --user -u jeryu.service`) has one
  `tool-finder: scheduled scan ...` line naming the effective cadence.
- Edge proxy: a line starting `jeryu-api proxy trust: requests arrive with
  X-Forwarded-For from <peer>, which is not in JERYU_TRUSTED_PROXIES` means the
  proxy's address is missing. The admin runners page says the same, and
  `GET /api/v1/control-plane/runners` reports `proxyTrust.trustedProxies` (how
  many addresses parsed) and `proxyTrust.message`.
- `jeryu-api: JERYU_MCP_PERSONAL_TOKEN_PEERS ignores ...` names an entry that
  was dropped.
- Sign-up: `GET /api/v1/auth/signup/policy` answers the effective
  `{"mode","waitlistUrl"}`. A line starting `jeryu-api sign-up:` names a
  `JERYU_SIGNUP` or `JERYU_WAITLIST_URL` value that was ignored.
- Mail has no startup line. Ask for a password reset for an account with a
  proved address: a spool directory gains an `.eml` file, and the audit log
  records `account.password_reset.requested`; a `failed` phase means a piece is
  missing.

## Gotchas

- `NAME=` and unset are the same for every live line of the example. The
  `# NAME=` lines are the exceptions, where an empty value breaks or weakens
  the forge: `JERYU_DATA_DIR` (refused at startup), `JERYU_BOT_SIGNING_KEY`,
  `JERYU_BOOTSTRAP_ADMIN_PASSWORD` (applied on every start),
  `JERYU_GITHUB_API_TOKEN` (an empty one hides `JERYU_GITHUB_API_TOKEN_FILE`),
  `JERYU_FLOW_EXPECTED_*` (reported invalid), and the process variables
  `HOME`, `PATH`, `TMPDIR`, `XDG_DATA_HOME`, `GITD_GIT_BIN`.
- `JERYU_BAD_TOKEN_RATE_LIMIT=0` does not disable that cap: it refuses every
  request carrying a credential. Leave it empty for the default.
- An unparsable interval (`JERYU_GITHUB_MIRROR_RECONCILE_MINUTES`,
  `JERYU_GITHUB_PULL_IMPORT_MINUTES`, `JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS`,
  `JERYU_CODEGRAPH_REFRESH_INTERVAL_HOURS`)
  turns that loop off rather than using the default.
- `JERYU_BASE` in the forge's file is `host:port` with no scheme. The
  release-board collector's own `JERYU_BASE` (`docs/release-board.md`) is a
  full URL; keep the two in their own files.
- `JERYU_DEPLOYERS` needs the login to be an admin too; unset, nobody can
  record a deployment.
- Secrets go in files the forge reads by path (`JERYU_GITHUB_API_TOKEN_FILE`,
  `JERYU_BOT_SIGNING_KEY`), mode 600, not inline in the env file.

## Not in this file

- **Release and unit tooling** under `scripts/release/` reads its own
  variables from the site limits file (`JERYU_FORGE_MEMORY_HIGH`,
  `JERYU_FORGE_MEMORY_MAX`, `JERYU_FORGE_TASKS_MAX`,
  `JERYU_FORGE_OOM_SCORE_ADJ`; see `scripts/release/README.md`) and from the
  caller's shell (`JERYU_HOME`, `JERYU_DATA`, `JERYU_SERVE_BIND`,
  `JERYU_HEALTH_URL`, `JERYU_*_TOKEN_FILE`, ...). Each script's header lists
  them.
- **Build time**: `JERYU_WEB_DIST` / `JERYU_WEB_DIST_LOCAL` (the web UI to
  embed) and `JERYU_BUILD_COMMIT` are read by `cargo build`, not by the server
  (`crates/jeryu-api/build.rs`).
- **The CLI client**: `JERYU_API_URL`, `JERYU_TOKEN`, `JERYU_TOKEN_FILE` belong
  to whoever runs `jeryu` commands against the forge.
- **Runner sandbox crates** linked into the binary also read
  `JERYU_RUN_OCI`, `JERYU_OCI_RUNTIME`, `JERYU_AGENT_IMAGE`,
  `JERYU_AGENT_SECCOMP_DIR`, `JERYU_MODEL_EGRESS_CONFIG` and
  `JERYU_TOOLCHAIN_HOME`. The server never reaches them on this release:
  managed sessions are refused until a Firecracker runner is verified
  (`docs/workcell.md`).
