# 0007. Let a bot reach whole owners and name its own kind

Status: Accepted
Date: 2026-10-12
Supersedes: none
Superseded-by: none

## Context

Record 0005 made a bot the unit of agent access: the owner's non-admin rights
cut down by a reach, checked on every request, and carried by scoped agent
tokens on REST, git and MCP. It left two parts of the bot as they were.

- A reach was `general` (every repository the owner is granted), a list of at
  most 100 repositories, or one task. Work is often organized by owner: a family
  of repositories under one account or organization, with new ones added over
  time. `general` is wider than the family; a list is stale the day a
  repository is added and has to be edited by hand.
- A bot's kind was `grok` or `muse`, enforced by a database `CHECK`. The kind
  grants nothing; it only labels the bot and picks what the legacy setup script
  prints. Agents of other makes had to pretend to be one of the two.

## Decision

1. **Owners reach.** A reach may be `{"mode":"owners","owners":["acme"]}`: every
   repository whose owner is listed and on which the bot's owner holds an
   explicit grant, including repositories created after enrollment. It is
   evaluated per request, like every reach. A public read without a grant is
   not enough, the same rule a `general` bot's roster uses. Enrollment trims,
   lowercases and deduplicates the names, takes 1 to 20 distinct owners, and
   refuses an owner unknown to the forge (`invalid_reach_owners`). A
   repository's bot roster shows such a bot as reaching that repository only.
2. **Free-form kind.** A kind is any label matching `^[a-z][a-z0-9-]{0,31}$`
   (`invalid_bot_kind` otherwise), `agent` when an enrollment names none.
   `grok` and `muse` keep their meaning and their display names, Grokbot and
   Musebot; any other kind is displayed as given. The legacy setup script
   writes an env file for `muse` and prints an `mcpServers` entry for every
   other kind, with the shared-computer warning for `grok` only.
3. **Schema.** jeryu-core migration 0021 rebuilds the `bots` table without the
   `grok`/`muse` `CHECK`, keeping every bot and every key, token and activity
   row. Applied migrations are not edited.

Everything in 0005 stands: agent tokens carry the bot's grant, and nothing in
this record widens what a grant can contain beyond the owner's non-admin rights.

## Consequences

- A family bot is enrolled once and follows the family as repositories are
  added and as the owner's grants change, with no list to maintain.
- An owners reach is wider than a list. Granting the owner a new repository
  under a listed owner puts it in reach of the bot at once; owners who want a
  fixed set keep using `repositories`.
- Older application builds cannot load a bot whose kind they do not know, and
  the 0021 rollback keeps the new table. Rolling back past 0021 after such a bot
  exists means recovering forward.
- `BotKind` is a validated string in jeryu-core, no longer an enum: Rust
  callers use `BotKind::GROK`, `MUSE` and `AGENT`, and clients must accept any
  kind in API responses.
