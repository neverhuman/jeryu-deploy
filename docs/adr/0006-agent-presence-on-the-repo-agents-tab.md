# 0006. Show every working agent on the repository's Agents tab

Status: Accepted
Date: 2026-10-12
Supersedes: none
Superseded-by: none

## Context

Agents work on a repository from many places: ad-hoc CLI sessions of several
vendors on operators' machines, queue workers, pull request reviewers, gate
runners, and jeryu's own agent runs. The forge saw only the last three, each
on a different page, and the CLI sessions not at all. Two agents could edit
the same branch of the same repository without anyone seeing it until the
push collided.

Some work is scoped to a whole family rather than one repository: a session
that refactors across a family touches repositories one by one, and the set
is not known up front.

`/api/v1/agent-runs*` is admin-only by path and names jeryu's own runs;
"sessions" names the repo-scoped launch route. A new name was needed.

## Decision

1. **One write route for reported sessions.** `POST /api/v1/agent-presence`
   upserts a whole session record keyed by a reporter-chosen `session_id`;
   `DELETE /api/v1/agent-presence/:session_id` ends it. A record carries a TTL;
   past it, it reads as stale. Ended and stale records stay listed for ten
   minutes, then drop off.
2. **Authorization is the repository's.** An ordinary login reports only
   repositories it can write, and a family only when it can write every
   hosted repository of it. Admins and the logins in
   `JERYU_PRESENCE_REPORTERS` (a sweeper on an operator's host) report any
   scope. A session belongs to the login that registered it.
3. **Fan out at read time.** A family-scoped record is stored once and
   matched against a repository's family when the repository's list is read,
   so it appears and disappears on every repository of the family at once.
4. **Merge the forge's own sources into the same list.** The repository read
   adds jeryu's running agent runs, gate runners and reviewers on the
   repository, and working queue slots, so the Agents tab is one list with a
   `source` per item and collisions marked across all of them.
5. **Hosts are for admins.** A reported `host` and the runner and slot hosts
   are returned to global admins only; forge-side items use opaque ids.

## Consequences

- The Agents tab answers "who else is on this branch" for every kind of
  agent, at the cost of a read that consults four stores. Each is in memory
  or one SQLite query; the todo queue is read only when a slot of the family
  is working.
- Reported data is self-description: a reporter can claim a title, branch or
  cost that is wrong. Scope is checked against write access, so a login can
  only paint repositories it could push to anyway.
- A family session collides with nothing: the record does not say which
  repository it is on at the moment. Reporters that know should send `repos`.
- jeryu's own runs keep no start or end time, so finished runs are left out
  of this list and running ones carry no `started_at`. The agent-runs list
  remains the place for their history.
- Session cost events for the ledger are follow-up work, not part of this
  route.
