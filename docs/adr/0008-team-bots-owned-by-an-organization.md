# 0008. Let an organization own a bot the team shares

Status: Accepted
Date: 2026-10-13
Supersedes: none
Superseded-by: none

## Context

Records 0005 and 0007 made a bot the unit of agent access. A bot belongs to one
person: `bots.owner` references `user_accounts(login)`. Its rights are that
person's explicit, non-admin grants, cut down by its reach. Only that person
manages its tokens, and an account event of that person (password change,
disable, lock) ends every one of them. That is right for an agent someone runs
for themselves.

A team also runs automation that belongs to no one in it: a reviewer, a merger,
a chat assistant. Today that automation can be one of two things, and neither
fits.

- **A login account.** It has a password and personal access tokens that carry
  every right it has, it holds grants like a person, and it shows up wherever
  people are listed. Telling it apart from a person takes a separate mark (the
  account kind kept in site settings), which has to be set by hand and is
  easy to miss.
- **A bot owned by one person.** That person's password reset takes the team's
  automation offline. If they leave, it goes with them. Its rights grow and
  shrink with their grants, and nobody else can rotate its tokens.

Record 0009 gives organizations owners and lets them run teams of people.
Neither confers anything: repository access is decided only by grants held by
a login (`repo_access_for`).

## Decision

1. **Two kinds of bot owner.** A bot is owned by a person, exactly as in 0005
   and 0007, or by an organization. A bot owned by an organization is a team
   bot. An organization-owned bot is never shown as owned by, or acting for,
   any one person.
2. **A team bot holds its own grants.** Its rights are grants made to the bot
   itself on single repositories, `read` or `write`, never `admin`. Someone
   with admin on that repository, or a forge administrator, makes and removes
   them, and each grant records who made it. Membership of the organization
   gives the bot nothing. Its reach narrows those grants as reach narrows a
   person's: `general` is every repository it holds a grant on, and `owners`
   (0007) is every repository under the listed owners on which it holds one.
3. **Stewards manage it.** At enrollment, a team bot names one team of its
   organization as its stewards. Members of that team, and forge
   administrators, may change its reach, suspend or revoke it, and issue or
   revoke its tokens. Being a steward grants no repository access, through the
   bot or otherwise.
4. **Tokens outlive any one steward's account changes, not their departure.**
   A team bot's token records who issued it. A steward's password change does
   not end tokens. Disabling or locking a steward's account, or removing them
   from the steward team, revokes the tokens that steward issued, because they
   may still have a copy; tokens issued by other stewards keep working. A
   steward can revoke every token they issued in one action. Suspending or
   revoking the bot ends all of its tokens, as in 0005. A team bot
   authenticates with agent tokens only: its enrollment key is never
   exchangeable, so no credential a steward once saw outlives their
   stewardship.
5. **A team bot is never a person.** It acts, authors and is audited as
   `<organization>.<slug>[bot]`, a name no account can have, and that name
   resolves to the bot's own grants wherever a login's access is checked. It
   is never offered as an assignee or reviewer, never counted among people,
   and needs no account kind. The
   account-kind mark stays only for automation that must be a login account,
   such as one that needs forge administration, which no bot can hold.
6. **Schema.** A jeryu-core migration, numbered when it is written, rebuilds
   `bots` with an owner kind (`user` or `organization`) and the steward team,
   keeping every bot and every key, token and activity row. It adds a table of
   bot repository grants and an `issued_by` on agent tokens. Applied
   migrations are not edited.
7. **Moving existing automation.** A person's bot that the team relies on is
   re-enrolled as a team bot, and the old bot is revoked once the new one
   works. There is no ownership transfer: a transfer would change where the
   bot's rights come from without anyone granting them.

Everything else in 0005 and 0007 stands. Agent tokens still carry only the
bot's grant, and only through `/mcp`, REST and git.

## Consequences

- The team's automation survives people leaving, password resets and changes
  to any one person's access, and more than one person can rotate its tokens.
- A team bot's access is listed on the bot and on each repository it can reach,
  instead of following from one person's grants. The price is that a new
  repository needs a grant before the bot can work on it, even under an
  `owners` reach. A later record may let an organization grant on all its
  repositories at once; this one does not.
- Team membership becomes security-relevant for the first time: adding
  someone to a steward team lets them mint the bot's tokens. Membership changes
  on a steward team are audited, and only organization owners and forge
  administrators make them.
- Automation that needs forge administration stays a login account marked as
  automation. Any reduction of those accounts is a separate decision.
- Older application builds cannot load an organization-owned bot. Rolling
  back past the migration after one exists means recovering forward.
- Clients that read `bot.owner` as a login must read the owner kind first.
