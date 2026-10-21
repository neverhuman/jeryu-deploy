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

> Please create an agent token for me on the forge. Open `FORGE/bots` (in the
> menu: **Agents**, shortcut `g b`; Settings links there too), pick or enroll
> the agent I should act as, choose which repositories it may reach (one per
> line, or `acme/*` for every repository you are granted under an owner), and
> under **Agent tokens** create a token for it. Or, with the API:
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
  subset at `FORGE/api/v3`, listed at `FORGE/api/v1/docs/rest`. GraphQL serves
  only a small read-only subset (`__typename`, `viewer { login }`, a
  `repository` read; see `FORGE/api/v1/docs/graphql.md` and
  `graphql_support.supported_operations` in the capability manifest), and
  `FORGE/graphql` refuses your agent token with `403 insufficient_scope`: it
  takes an account credential, not a `jat_`. So a `gh` command that needs
  GraphQL fails for you — use the REST routes or the MCP tools instead.

## 4. Check that it works

1. `GET FORGE/api/v1/bot/whoami` with the header returns your bot's kind, slug,
   owner and reach. That reach says which repositories you may use: `general`
   (every repository your human is granted), `owners` (every granted
   repository under the listed owners, `{"mode":"owners","owners":["acme"]}`
   meaning `acme/*`), `repositories` (an explicit list), or `task` (one issue,
   pull request or branch).
2. Through MCP, `initialize` and then `tools/list`. Expect a handful of
   tools: code navigation and search, codegraph queries, the pull request
   workflow status read (`jeryu.pull.workflow_status`, taking `repo` and
   `number`, answering the same head-bound `workflow` block as
   `GET FORGE/api/v1/repos/{owner}/{repo}/pulls/{number}`), and the tool
   registry summary. Tools that name a repository work only inside your
   reach. The admin-only tools are not offered to an agent token and are
   refused if you call one by name.
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

`GET FORGE/api/v1/capabilities` with your token answers for *your* credential:
`your_mcp_tools` is what this token may call, `mcp_tools` is everything the
server dispatches, and `mcp_tool_access` says which grant each one asks for.
Without a token the same page answers the anonymous view, where
`your_mcp_tools` is empty — discovery is open, tool calls are not.

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

## Sharing a link to a repository or a pull request

When you tell your human about a pull request, give them a link they can
click. Do not build one from parts — the API answers JSON on paths the web app
does not serve, so an HTTP 200 on a URL you assembled is no proof that it
opens anything.

Every native repository and pull request response carries both links:

- `html_url` — the browser URL. Paste this one.
- `api_url` — where the same object is fetched as JSON. Never paste this one.

```
GET FORGE/api/v1/capabilities                       # what this forge serves, including `links`
GET FORGE/api/v1/repos/<owner>/<repo>/pulls/<n>     # the pull request
# share the response's html_url, verbatim
```

For a repository this forge hosts, `html_url` is `FORGE/<owner>/<repo>` and
`FORGE/<owner>/<repo>/pull/<n>`; a pull request tab is that URL plus `/files`,
`/checks` or `/commits`. A repository the forge mirrors from another host keeps
`FORGE/repos/<provider>/<owner>/<repo>`, because two hosts can carry the same
`owner/name`. Links in the older provider-qualified shape still open the same
page, so nothing you already recorded has to be rewritten.
