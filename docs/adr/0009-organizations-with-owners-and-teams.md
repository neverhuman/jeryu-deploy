# 0009. Give organizations owners and let them run their teams

Status: Accepted
Date: 2026-10-13
Supersedes: none
Superseded-by: none

## Context

jeryu-core has had organizations and teams since its first schema: an
organization is a name, and a team is a named list of member logins under one
organization. Nothing on a running forge can create either. No route creates an
organization or a team, adds or removes a member, or names who may do so; only
tests and invitation team bindings touch them. An organization has no owner, so
there is no one to ask.

Repository owners such as a family's namespace are plain names on repositories,
not organization rows, and access to a repository is decided only by grants
held by a login.

Record 0008 needs a group of people that outlives any one of them: the stewards
of a bot the team shares. That group has to be something people can create, see
and change, with a clear answer to who may change it.

## Decision

1. **An organization is a named group of people with owners.** Its login is
   canonical (lowercase letters, digits, hyphen, underscore) and unique across
   accounts, user profiles and organizations. It may equal an existing
   repository owner name; naming it does not move or grant those repositories.
2. **Forge administrators create organizations** and name at least one owner.
   Every owner is an active account.
3. **Owners run the organization.** An owner, or a forge administrator, may add
   or remove owners, create teams, and add or remove team members. The last
   owner cannot be removed. An organization created before this record has no
   owners and is run by forge administrators until one is added.
4. **A team is a named list of people.** Its slug is unique within its
   organization, and every member it gains is an active account. A team is
   emptied, not deleted, so a reference to it never dangles.
5. **Membership is audited.** Creating an organization or team and every owner
   or member change records who made it, on what, and for whom.
6. **Neither grants repository access.** Owning an organization or belonging to
   a team gives no repository right, no forge administration, and nothing else
   except what a record names. Record 0008 names one: stewarding a team bot.
7. **Who sees it.** An organization, its owners and its teams are visible to the
   organization's owners, its team members, and forge administrators.
8. **Storage.** Owners and audit fields are added to the stored organization
   document. Existing rows read as organizations with no owners. No migration
   is needed.

## Consequences

- People can set up a group once and change it as the team changes, without a
  forge administrator for every member change.
- An organization owner can make someone a steward of the organization's team
  bots by adding them to the steward team. Owners are trusted with that, and
  the audit log shows every change.
- Repository access stays exactly where it was. Granting a team access to
  repositories, or letting an organization own repositories by grant, needs a
  record of its own.
- Organizations and teams are never deleted under this record. Removing one
  later must first decide what happens to the bots and invitations that name
  it.
