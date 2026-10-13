# Account bots (agents)

A bot is a credential that an account owns and hands to an agent. Every bot
has a **kind**: a free-form label naming what drives it. `grok` (shown as
Grokbot) is meant for a general-purpose agent that drives MCP; `muse` (shown
as Musebot) for a worker that runs from an env file; any other kind, such as
`agent` (the default) or `claude-code`, is shown as given. A kind matches
`^[a-z][a-z0-9-]{0,31}$`. Every kind authenticates the same way and gets the
same rights: neither the kind nor the display name ever authorizes anything.

## What a bot may do

A bot gets **the owner's non-admin rights, cut down by its reach**, checked
again on every request against the owner's current grants.

- **Reach** is one of:
  - `general`: every repository the owner has an explicit grant on;
  - `owners`: every repository of 1 to 20 listed owners (accounts or
    organizations) on which the owner has an explicit grant, including
    repositories created later, for example
    `{"mode":"owners","owners":["acme"]}` for everything granted under
    `acme/*`. A public repository without a grant is not in reach. Names are
    trimmed, lowercased and deduplicated, and each must exist on the forge;
  - `repositories`: a list of at most 100 `{owner, name}` pairs;
  - `task`: one issue, pull request or branch in one repository. Writes must
    target that task; reads anywhere in that repository are allowed.
- Forge admin is never inherited. A bot cannot mint keys, change the
  repository itself (archive, rename, delete, settings), edit branch
  protection, transfer a repository, publish statuses or check runs, or
  record deployments.
- A bot reaches only `/api/v1/repos/*`, `/api/v1/bot/whoami`, git over HTTPS,
  the GitHub-compatible `/api/v3/repos/*` surface, and MCP. On MCP it sees
  no admin tool, and every repository argument is checked against its reach.
- Changing the owner's password, or disabling or locking the account, revokes
  every key the account's bots hold.

## Enroll

As the owner (session or personal access token):

```
curl -s -X POST https://forge.example/api/v1/bots \
  -H "Authorization: Bearer $(cat ~/.config/jeryu/owner.pat)" \
  -H 'Content-Type: application/json' \
  -d '{"slug":"atlas","display_name":"Atlas","kind":"grok",
       "reach":{"mode":"repositories","repos":[{"owner":"acme","name":"widgets"}]}}' \
  > atlas.enroll.json && chmod 600 atlas.enroll.json
```

`kind` may be left out; it is then `agent`. A kind that does not match
`^[a-z][a-z0-9-]{0,31}$` is refused with `invalid_bot_kind`, and an owners
reach that is empty, names more than 20 distinct owners, or names an unknown
owner with `invalid_reach_owners` (both 422). A whole family:

```
  -d '{"slug":"family","display_name":"Family",
       "reach":{"mode":"owners","owners":["acme"]}}'
```

A repository's bot roster shows an owners bot as reaching that repository
only; the other owners it lists are not shown there.

The response holds `bot`, `key_id`, `enrollment_key` and `setup_script`. The
key is shown once. It stays valid until it is rotated or revoked, so treat it as
the bot host's long-lived secret. Keep the response out of logs and shell
history.

`GET /api/v1/bots` lists the account's bots. `POST /api/v1/bots/{id}/keys`
rotates the key (the old one retires after a grace period),
`/api/v1/bots/{id}/suspend` pauses a bot, and `/api/v1/bots/{id}/revoke` ends
it. A repository's writers can see its bots at `GET /api/v1/repos/{id}/bots`
and their activity at `/api/v1/repos/{id}/bot-activity`.

## Agent tokens (recommended)

An agent token is the simplest way to connect an agent: one URL and one token,
no exchange and no local proxy. The token acts as the bot: it carries the bot's
grant and nothing more, on REST, git and MCP alike
(see `docs/adr/0005-one-mcp-endpoint-scoped-agent-tokens.md`).
To set an agent up, give it the token and the page
`/api/v1/docs/connect-an-agent.md` on this forge; it walks the agent through
MCP, REST and git itself (`docs/connect-an-agent.md`).

```
curl -s -X POST https://forge.example/api/v1/bots/<bot-id>/tokens \
  -H "Authorization: Bearer $(cat ~/.config/jeryu/owner.pat)" \
  -H 'Content-Type: application/json' \
  -d '{"label":"laptop","ttl_days":90}'
```

- The response holds `token` (shown once, starts with `jat_`), `token_id`,
  `label`, `created_at` and `expires_at`. `ttl_days` is 1 to 365, default 90.
  A bot holds at most 10 live tokens.
- `GET /api/v1/bots/<bot-id>/tokens` lists them (never the token, with
  `last_used_at`). `POST /api/v1/bots/<bot-id>/tokens/<token-id>/revoke` ends
  one at once.
- A token ends when it expires or is revoked, when the bot is suspended or
  revoked, or when the owner's password changes or the account is disabled or
  locked. Rotating the bot's enrollment key does not end it.
- A bot or agent token cannot issue tokens.

Use it directly:

- **MCP:** `https://forge.example/mcp` with the header
  `Authorization: Bearer <token>`. Any standard MCP client works; admin tools
  are not offered to it, and every repository it names is checked against the
  grant.
- **gh and git:** the token is a normal bearer or basic-auth password, for
  example `GH_TOKEN=<token>` or a git credential helper that returns it.

The setup script and local proxy below remain for hosts that already use them.

## Set up the bot host

On the machine that runs the agent, run the setup script from the response:

```
jq -r .setup_script atlas.enroll.json > atlas-setup.sh
bash atlas-setup.sh && rm atlas-setup.sh atlas.enroll.json
```

or print it with the CLI, which takes the key from stdin, a file, or
`BOT_ENROLLMENT_KEY`, never from an argument:

```
jq -r .enrollment_key atlas.enroll.json | \
  jeryu bot --kind grok --slug atlas --forge https://forge.example --enrollment-key-stdin | bash
```

The script writes `~/.jeryu/bots/<slug>/` (mode 0700), exchanges the key, and
starts a loopback-only proxy on an OS-assigned port. It does not touch
`~/.config/gh` or any shared token file.

- **Grokbot, and every kind other than `muse`:** the script prints an MCP
  server entry such as
  `{"mcpServers":{"jeryu":{"url":"http://127.0.0.1:<port>/mcp/<local-secret>"}}}`.
  Add it to the agent's MCP configuration. Only `grok` also warns that every
  Grokbot of the account shares the computer.
- **Musebot:** the script writes `~/.jeryu/bots/<slug>/jeryu-bot.env`; load it
  with `set -a; . ~/.jeryu/bots/<slug>/jeryu-bot.env; set +a`.
- **gh and git, either kind:** `. ~/.jeryu/bots/<slug>/client.env` points
  `GH_CONFIG_DIR` and `JERYU_MERGE_TOKEN` at the bot's own files.

Any standard MCP client works through the proxy. It fills in the MCP routing
headers (`Mcp-Method`, `Mcp-Name`, `MCP-Protocol-Version`) that clients on older
protocol versions leave out, copying them from the request and from the version
agreed at `initialize`.

The proxy refreshes the access token in the background, so gh and git keep
working without MCP traffic. To refresh by hand, run
`python3 ~/.jeryu/bots/<slug>/broker.py refresh-clients`. The proxy does not
restart after a reboot: rerun the setup script. A rerun with the same key
keeps the cached refresh token.

## Tokens

- `POST /api/v1/bot/token` `{enrollment_key, aud}` returns a 15-minute HS256
  access token and a refresh token. `aud` must be the forge origin the bot calls.
- `POST /api/v1/bot/token/refresh` `{refresh_token, aud}` rotates the pair.
  A refresh token lives 12 hours. Presenting one that was already rotated
  revokes the whole family; exchange the enrollment key again to recover.
- Only the access token is a bearer. An enrollment key or refresh token sent as
  `Authorization: Bearer`, or any credential in a query string, is refused.

## Behind a reverse proxy

`/mcp` serves every caller and offers each one only the tools its principal
may use. An agent token sees no admin tool. A personal access token, which
carries every right of its account, is refused on `/mcp`; this trusts no request
header, so a public edge can expose `/mcp` as it is. A site that wants personal
tokens on MCP for local tools lists their direct peer addresses in
`JERYU_MCP_PERSONAL_TOKEN_PEERS` (empty by default). Never list a proxy's
address, and note that a proxy on the same host connects from loopback. List single
addresses: ranges and the unspecified address (`0.0.0.0`, `::`) are refused
with a warning at startup. For the same reason, never enable local-dev trust
(`JERYU_WEB_TRUST_LOCAL`) on a forge an edge exposes. Requests carrying a
forwarding header are refused local-dev trust as a second line of defense
only; a proxy that adds no such header still looks local. `/mcp/bot` is the same endpoint for bot access tokens and
stays for existing bot hosts. An edge that still blocks `/mcp` can keep only the
bot mount open:

```
@mcp_private {
    path /mcp /mcp/*
    not path /mcp/bot /mcp/bot/*
}
handle @mcp_private {
    respond 404
}
```

Token routes (`/api/v1/bot/token*`) are public by design and rate limited per
client address. Set `JERYU_TRUSTED_PROXIES` to the proxy's address so those
limits key on the real client and not on the proxy. The variable is unset by
default: the forge then ignores `X-Forwarded-For` and every token request
shares the proxy's one bucket. The first such request writes one warning
naming `JERYU_TRUSTED_PROXIES`, and `GET /api/v1/control-plane/runners`
carries the same sentence in `proxyTrust` for the admin runners and system
pages, with a count and the peer each limit keyed on. Owner step: set
`JERYU_TRUSTED_PROXIES` to the edge proxy's address in the forge's site env
file on the forge host and restart the forge.

## Team bots

A team bot belongs to an organization, not to a person (ADR 0008). Use one
for automation the team shares: it keeps working when anyone leaves, resets a
password, or changes their own access. Organizations, owners and teams are
described in `docs/organizations.md`.

- **Enroll.** An owner of the organization, or a forge administrator, names a
  team of that organization as the bot's stewards:

  ```bash
  curl -sS -X POST "$FORGE/api/v1/orgs/acme/bots" \
    -H "Authorization: Bearer $PAT" -H 'Content-Type: application/json' \
    -d '{"steward_team":"stewards","slug":"reviewer","display_name":"Reviewer"}'
  ```

  `kind` and `reach` are optional (`agent`, `general`). The answer has no
  enrollment key: a team bot authenticates only with agent tokens.
- **Rights.** Only grants made to the bot itself, on single repositories,
  `read` or `write`, never `admin`. An administrator of the repository (or a
  forge administrator) makes them:
  `PUT /api/v1/bots/{id}/grants/{owner}/{repo}` with `{"access":"write"}`, and
  `DELETE` the same path to remove one. Its reach narrows those grants as for
  any bot. Organization membership gives it nothing.
- **Name.** It acts, authors and is audited as `<organization>.<slug>[bot]`,
  for example `acme.reviewer[bot]`. No account can have that name; repository
  checks resolve it to the bot's own grants.
- **Stewards.** Members of the steward team, and forge administrators, issue
  and revoke its agent tokens, rotate its key, suspend or revoke it, and read
  `GET /api/v1/bots/{id}/grants`. The organization's owners steward it only
  from the team. `GET /api/v1/bots` lists the caller's team bots under
  `team_bots`.
- **Departures.** Each agent token records who issued it and works only while
  that person may still steward the bot. Removing someone from the steward
  team, or disabling or locking their account, revokes the tokens they issued;
  a password change does not. `POST /api/v1/bots/tokens/revoke-issued` revokes
  every team bot token the caller issued.

## Check

- `GET /api/v1/bot/whoami` with the bot's access token returns its kind, slug,
  owner, owner kind, acting login and reach.
- A read inside the reach returns 200; a private repository outside it returns 403.
- Errors carry a machine code and a repair hint; see `docs/errors.md`.
