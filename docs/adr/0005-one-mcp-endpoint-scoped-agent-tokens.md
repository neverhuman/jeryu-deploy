# 0005. Serve one MCP endpoint and authorize agents with scoped tokens

Status: Accepted
Date: 2026-10-12
Supersedes: none
Superseded-by: none

## Context

The forge serves MCP at `/mcp`. Its tool catalog mixes code navigation with
admin tools (control-plane telemetry, the tool-finder scan, agent-run control
that starts processes on the host). A personal access token carries every right
its account has, admin included, so a public edge had to keep `/mcp` closed.

Account bots (Grokbot, Musebot) were the first way out. A bot is a grant: the
owner's non-admin rights cut down by a reach (every granted repository, a list
of repositories, or one task), checked again on every request. The mechanics
around that grant grew heavy:

- A bot exchanges a long-lived enrollment key for a 15-minute access token and
  a rotating refresh token. Standard MCP clients cannot refresh a bearer, so
  every bot host runs a local proxy that holds the refresh token, attaches a
  fresh access token, and is restarted after a reboot by rerunning a setup
  script.
- The long-lived enrollment key sits on the same host as the proxy, so the
  short access token buys little: whoever reads that host can mint new ones.
- Bots use a second mount, `/mcp/bot`, which differs from `/mcp` only in the
  tools it shows. Which tools a caller sees is a property of what the caller may
  do, not of the URL.
- The proxy also papers over server gaps: it fills routing headers the server
  required, and the server refused any protocol version but its own.

## Decision

1. **One endpoint.** `/mcp` serves every caller. The tools a caller sees, and
   the ones it may call, follow from its principal: a browser session or an
   admin login sees the admin tools; an agent token sees only tools that act
   inside its grant, and every repository argument is checked against that
   grant. `/mcp/bot` stays as an alias while existing bot hosts move over.
2. **Scoped agent tokens.** An owner issues a token for one of its bots. The
   token is presented directly as `Authorization: Bearer` on REST, git and MCP.
   It adds no rights of its own: it resolves to the bot's grant and is
   authorized exactly like the bot. It has an expiry (default 90 days, at most
   365), an optional label, a last-used time, and can be revoked at any time.
   Suspending or revoking the bot, or an account event of its owner (password
   change, disable, lock), ends all of the bot's tokens. Only the token's
   SHA-256 is stored. Tokens start with `jat_` so secret scanners can find them.
3. **Unscoped tokens stay off MCP.** A personal access token, which carries
   every right of its account, is refused on `/mcp`. The decision trusts no
   request header: a proxy header is client-settable and a proxy may not add
   it, so "came through a proxy" cannot be the test. A site may list direct
   peers (the TCP peer of the connection) whose personal tokens MCP accepts,
   with `JERYU_MCP_PERSONAL_TOKEN_PEERS`; the list is empty by default and must
   never contain a proxy's address. Loopback is not implied, because a proxy on
   the same host connects from loopback.
4. **Standard clients connect directly.** The server negotiates the protocol
   version, takes the routing method from the request body when the header is
   absent (a header that contradicts the body is still refused), and ends a
   session on `DELETE`. A client needs only the URL and a token.
5. **OAuth comes later, on the same token.** If browser-hosted connectors are
   needed, an authorization-code flow will issue the same scoped tokens. It is
   not a second authorization model.

The enrollment-key exchange, short access tokens and the setup-script proxy keep
working, unchanged, for hosts that already use them.

## Consequences

- An agent is set up with one URL and one token. No proxy, no setup script, no
  restart after a reboot.
- A leaked agent token is valid until it expires or is revoked, not for 15
  minutes. Narrow reach, expiry, last-used and instant revocation limit that;
  the enrollment key, which had the same exposure, already worked this way.
- Opening `/mcp` on a public edge becomes safe once this ships: no public caller
  reaches an admin tool without an admin login. Local-dev trust (an
  unauthenticated loopback caller acting as the local admin) must never be
  enabled on a forge an edge exposes: a proxy on the same host connects from
  loopback. As defense in depth only, the API, the GitHub edge and git
  transport withhold local-dev trust from any request carrying a forwarding
  header (`Forwarded`, `Via`, `X-Forwarded-*`, `X-Real-IP` and the common CDN
  variants); a proxy that sends none of them still looks local.
- Secrets that are shown once (enrollment keys, agent tokens, exchanged tokens,
  personal access tokens, session cookies) are never kept by the idempotency
  store: their answer is kept as a placeholder, and no kept answer carries
  `Set-Cookie`.
- Existing personal-token MCP callers on the forge host stop working until the
  operator lists their peer address in `JERYU_MCP_PERSONAL_TOKEN_PEERS`; moving
  them to agent tokens is the intended path.
- The tool surface for agents is now the work: pull requests, reviews, checks
  and the attention feed belong on the one endpoint, filtered by grant.
- `/mcp/bot`, the exchange routes and the setup-script proxy become deprecated
  and can be removed once no bot host uses them.
