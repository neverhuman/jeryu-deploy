# Grokbots and Musebots

A bot is a credential that an account owns and hands to an agent. A Grokbot
(`grok`) is meant for a general-purpose agent that drives MCP. A Musebot
(`muse`) is meant for a worker that runs from an env file. Both kinds
authenticate the same way and get the same rights. The display name is a
label only; it never authorizes anything.

## What a bot may do

A bot gets **the owner's non-admin rights, cut down by its reach**, checked
again on every request against the owner's current grants.

- **Reach** is one of:
  - `general`: every repository the owner has an explicit grant on;
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

The response holds `bot`, `key_id`, `enrollment_key` and `setup_script`. The
key is shown once. It stays valid until it is rotated or revoked, so treat it as
the bot host's long-lived secret. Keep the response out of logs and shell
history.

`GET /api/v1/bots` lists the account's bots. `POST /api/v1/bots/{id}/keys`
rotates the key (the old one retires after a grace period),
`/api/v1/bots/{id}/suspend` pauses a bot, and `/api/v1/bots/{id}/revoke` ends
it. A repository's writers can see its bots at `GET /api/v1/repos/{id}/bots`
and their activity at `/api/v1/repos/{id}/bot-activity`.

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

- **Grokbot:** the script prints an MCP server entry such as
  `{"mcpServers":{"jeryu":{"url":"http://127.0.0.1:<port>/mcp/<local-secret>"}}}`.
  Add it to the agent's MCP configuration.
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

MCP for bots is served at `/mcp/bot`. That mount hides every admin tool and
enforces reach, so a public edge can expose it while keeping the general
`/mcp` endpoint private:

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

## Check

- `GET /api/v1/bot/whoami` with the bot's access token returns its kind, slug,
  owner and reach.
- A read inside the reach returns 200; a private repository outside it returns 403.
- Errors carry a machine code and a repair hint; see `docs/errors.md`.
