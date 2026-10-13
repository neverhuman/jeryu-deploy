# Organizations and teams

An organization is a named group of people with owners; a team is a named list
of people inside it (ADR 0009). Neither grants a repository right. Their one
use today is stewarding team bots (`docs/bots.md`, ADR 0008).

## Who may do what

| Action | Who |
|---|---|
| Create an organization (`POST /api/v1/orgs`) | forge administrator |
| Add or remove an owner (`PUT`/`DELETE /api/v1/orgs/{org}/owners/{login}`) | an owner, or a forge administrator; never the last owner (`409 last_owner`) |
| Create a team (`POST /api/v1/orgs/{org}/teams`) | an owner, or a forge administrator |
| Add or remove a member (`PUT`/`DELETE /api/v1/orgs/{org}/teams/{team}/members/{login}`) | an owner, or a forge administrator |
| See it (`GET /api/v1/orgs`, `GET /api/v1/orgs/{org}`) | its owners, its team members, forge administrators; anyone else gets `404` |

Owners and members are active accounts. An organization's login is unique
across accounts and organizations, and may equal a repository owner name
without moving or granting those repositories. Teams are emptied, never
deleted. Every change is in the audit log under `org:<org>` or
`team:<org>/<team>`.

Removing a member from a team also revokes the agent tokens that person
issued for the team bots the team stewards.

## Create one

```bash
curl -sS -X POST "$FORGE/api/v1/orgs" -H "Authorization: Bearer $ADMIN_PAT" \
  -H 'Content-Type: application/json' \
  -d '{"login":"acme","display_name":"Acme","owners":["olive"]}'
curl -sS -X POST "$FORGE/api/v1/orgs/acme/teams" -H "Authorization: Bearer $OWNER_PAT" \
  -H 'Content-Type: application/json' -d '{"name":"Stewards","members":["sam","tess"]}'
```

## Check

- `GET /api/v1/orgs/acme` answers the organization, its teams, and
  `can_manage` for the caller.
- A stranger's `GET /api/v1/orgs/acme` is `404`, not `403`.
