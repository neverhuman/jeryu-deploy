# Connect an agent to this forge

This page is written for an AI agent (Claude Code, Codex, Grok, or any MCP
client) whose human wants it to work on repositories hosted on this forge. If
you are that agent, follow it top to bottom. It takes about five minutes.

**The forge** is the origin you fetched this page from: the scheme and host of
this URL, without the `/api/v1/docs/...` path. Below it is written `FORGE`
(for example `https://forge.example`).

## 1. Get a token from your human

You need one **agent token**, a string that starts with `jat_`. You cannot
create it yourself; ask your human for one. Never use a personal access token
(`jpat_...`) or a password: MCP refuses personal tokens, and an agent token
carries only the access it should.

Tell your human exactly this:

> Please create an agent token for me on the forge. On your account's Bots
> page, pick (or enroll) the bot I should act as, choose which repositories it
> may reach, and create a token for it. Or, with the API:
> `POST FORGE/api/v1/bots/<bot-id>/tokens` with body
> `{"label":"<what I am>","ttl_days":90}` and your own credentials.
> The token is shown once; give it to me through a secret store or an
> environment variable, not in chat if you can avoid it.

Keep the token out of URLs, logs, commit messages and files you commit. Store it
in an environment variable such as `JERYU_TOKEN` or your client's secret store.

What the token can do: whatever your human may do on the repositories the bot
is granted, minus anything administrative. It cannot create tokens, change
repository settings, edit branch protection, transfer or delete repositories,
publish CI statuses, or use admin tools. It ends when it expires, when your
human revokes it, or when the bot is suspended.

## 2. Configure MCP

The MCP endpoint is `FORGE/mcp` (streamable HTTP). Authenticate every request
with the header `Authorization: Bearer <token>`. No proxy or helper is needed.

- **Claude Code:**

  ```
  claude mcp add --transport http jeryu FORGE/mcp --header "Authorization: Bearer $JERYU_TOKEN"
  ```

- **Any MCP client that takes JSON configuration:**

  ```json
  {
    "mcpServers": {
      "jeryu": {
        "url": "FORGE/mcp",
        "headers": { "Authorization": "Bearer <token>" }
      }
    }
  }
  ```

- **Other clients:** choose the streamable HTTP transport, URL `FORGE/mcp`,
  and add the `Authorization: Bearer <token>` header. Any protocol version your
  client offers is fine: the server answers with the version it speaks.

## 3. Configure REST, `gh` and git (same token)

- **REST:** send `Authorization: Bearer <token>` to `FORGE/api/v1/...` (this
  forge's API) or `FORGE/api/v3/...` (a GitHub-compatible subset).
- **git over HTTPS:** clone from `FORGE/git/<owner>/<repo>.git`. Use the token
  as the password (any user name), through a credential helper or
  `GIT_ASKPASS`, never inside the clone URL you save.
- **GitHub tools (`gh`, Octokit):** the forge answers a GitHub-compatible REST
  subset at `FORGE/api/v3`, listed at `FORGE/api/v1/docs/rest`. GraphQL is not
  served, so commands that need it fail; prefer the MCP tools or plain REST.

## 4. Check that it works

1. `GET FORGE/api/v1/bot/whoami` with the header returns your bot's kind, slug,
   owner and reach. That reach is the list of repositories you may use.
2. Through MCP, `initialize` and then `tools/list`. Expect a handful of
   tools: code navigation and search, codegraph queries, and the tool
   registry summary. Tools that name a repository work only inside your
   reach.
3. `git ls-remote FORGE/git/<owner>/<repo>.git HEAD` for a repository in your
   reach prints a commit id.

Report the result of all three to your human.

## 5. When something fails

| You see | It means | Do this |
|---|---|---|
| 401 `invalid_token` or `invalid bot credential` | The token is wrong, expired or revoked | Ask your human for a new token |
| 403 `insufficient_scope` on `/mcp` | You sent a personal access token | Use an agent token (`jat_`) |
| 403 `repo_not_granted` | The repository is outside the bot's reach | Ask your human to widen the bot's reach, or work elsewhere |
| 403 `insufficient_scope` on a write | The action is not allowed for agents, or the owner lacks write | Ask your human; do not look for another credential |
| 400 on `/mcp` | Your client sent a routing header that contradicts the request body | Let the client set the headers itself, or send none |
| A tool returns `isError` with `requires repo` | The tool needs a `repo` argument | Pass `owner/name` of a repository in your reach |

Every error body carries `code`, a `message` and a repair hint; the full list
is at `FORGE/api/v1/docs/errors.md`. Background for your human:
`FORGE/api/v1/docs/bots.md` and
`FORGE/api/v1/docs/adr/0005-one-mcp-endpoint-scoped-agent-tokens.md`.

## Rules while you work

- Use only the token your human gave you, and never look for others on the
  machine.
- Do not share the token with other agents or tools you start; ask your human
  for a separate token for each.
- Pull requests are the way changes reach `main`. The forge reviews and merges
  them; you cannot push to `main`.
