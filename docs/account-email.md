# Account email, verification and password recovery

An account's password lives in the forge core. Its email address, whether that
address has been proved, and the single-use links that prove it or recover the
password live in `<data_dir>/shift.sqlite`
(`db/migrations/0017_account_emails.sql`), owned by
`crates/jeryu-api/src/web/account_email.rs`.

Before this surface existed an account carried no address at all, so somebody
who lost their password waited for an administrator to run
`POST /api/v1/admin/users/:login/reset-password`. That route still works and is
still the way back for an account with no proved address.

## Routes

| Route | Credential | Answer |
|---|---|---|
| `GET /api/v1/auth/email` | the account | `{"address": …\|null, "verified": bool}` |
| `PUT /api/v1/auth/email` | the account | `202 {"result":"sent","verified":false}` |
| `POST /api/v1/auth/email/verify/resend` | the account | `202 {"result":"sent","verified":false}` |
| `POST /api/v1/auth/email/verify` | none | `200 {"result":"verified","verified":true}` |
| `POST /api/v1/auth/password/forgot` | none | `202 {"result":"sent"}` |
| `POST /api/v1/auth/password/reset` | none | `200 {"result":"reset"}` |

The three routes that need no credential are the ones a mail client reaches:
a link is followed from a mailbox that holds no session, and the link itself
names the account.

`POST /api/v1/auth/password/forgot` answers `202 {"result":"sent"}` for every
well-formed request, byte for byte. Whether an account holds the address,
whether it proved it, whether the hourly cap was already spent and whether a
message actually left are all invisible to the caller and recorded in the
audit log instead. A malformed address is the same answer, because refusing it
would tell a caller which of its guesses this forge would even store.

An unspendable link — expired, already spent, never issued, or offered for the
wrong purpose — is one refusal: `422 reset_link_invalid` or
`422 verification_link_invalid`.

## What keeps a link safe

- The secret is 32 bytes of operating-system randomness. Only its sha256 is
  stored, so the database holds nothing spendable and neither does a backup.
- Spending is one `UPDATE … WHERE consumed_ms IS NULL AND expires_ms > now`,
  so a replay finds nothing.
- A verification link lasts a day; a reset link lasts an hour.
- A reset link is only ever sent to a **proved** address, so taking over an
  unproved address takes over no account.
- Claiming an address, proving one, or finishing a reset retires that
  account's other outstanding links.
- A finished reset goes through core's administrative reset, which rotates the
  account's auth epoch and drops its sessions, personal access tokens and
  bots. Recovering therefore signs the account out everywhere.
- A password below core's floor is refused *before* the link is spent, so a
  rejected password does not burn the person's one link.
- Nothing logs a secret, a message body, or an address: the rate-limit keys
  hold a digest of the address rather than the address, and `OutgoingMail`'s
  `Debug` redacts the body.

## Rate limits

Alongside the ordinary `/api/v1` read and bad-credential caps, all in the one
`auth_rate_limits` window map:

| Bucket | Cap |
|---|---|
| `POST …/password/forgot` per client address | 5 / hour |
| `POST …/password/forgot` per address asked for | 3 / hour |
| `POST …/password/forgot` for the whole route | 30 / minute |
| `POST …/password/reset` per client address | 10 / hour |
| `POST …/email/verify` per client address | 10 / hour |
| reset links issued per account | 5 / hour (in the store) |
| verification links issued per account | 5 / hour (in the store) |

The first five are in memory and a restart forgets them; the last two are
counted from `account_email_tokens`, so a restart loop cannot be used to send
somebody a hundred messages.

Each refusal is the ordinary `429` with `Retry-After` and the rate-limit
budget headers.

## Mail configuration

The forge carries no SMTP client and depends on no mail network. A host that
sends mail already runs something that does, so a message leaves the process
one of two ways:

| Variable | Meaning |
|---|---|
| `JERYU_MAIL_FROM` | envelope and header sender, e.g. `jeryu <no-reply@example.com>` |
| `JERYU_MAIL_BASE_URL` | public origin links are built on, e.g. `https://jeryu.example.com` |
| `JERYU_MAIL_SENDMAIL` | a sendmail-compatible program to pipe the message to, e.g. `/usr/sbin/sendmail -t -i` |
| `JERYU_MAIL_SPOOL` | a directory to write one RFC 5322 `.eml` file into per message |

`JERYU_MAIL_SENDMAIL` wins when both transports are set. A spooled message is
written to `<name>.tmp` and renamed, so a path unit or cron job that submits
the directory never reads half a message. Retries belong to the program or the
job, not to this process.

`JERYU_MAIL_BASE_URL` is not guessed from the listener: a link that points at
the bind address is a link nobody can follow. The links built on it are
`<base>/reset-password?token=…` and `<base>/verify-email?token=…`, which the
web app serves.

**All three pieces are needed** — a sender, a base URL, and one transport.
With anything missing the forge sends nothing: every route answers exactly as
it would have, and the unsent message is recorded as a `failed`
`account.password_reset.requested` or `account.email.verification_sent`
record. A recovery request that discloses nothing must keep disclosing nothing
while an operator is half way through wiring mail, so an operator watches the
audit log, not the HTTP answer.

## Audit records

All under subject `account_email`, phase `requested` / `completed` /
`failed`:

| Action | When |
|---|---|
| `account.email.claimed` | an account stored an address |
| `account.email.verification_sent` | a verification link was sent, or was not |
| `account.email.verified` | an address was proved |
| `account.password_reset.requested` | a reset was asked for; `requested` when there was nothing to send to |
| `account.password_reset.completed` | a link was spent and the password set |

A detail payload never carries an address or a secret, so the trail is not an
address directory.

## Migration and backfill

`0017_account_emails` creates two empty tables and backfills nothing, by
design:

- Every account that already existed reads as having no address. Its sign-in,
  sessions, tokens and bots are untouched — nothing in the authentication path
  consults these tables unless a route above is called.
- Such an account cannot start a recovery. `POST …/password/forgot` answers
  `202` for it like everything else and sends nothing.
- No backfill could put a trustworthy address here. An address is only ever
  written by the account's own signed-in `PUT /api/v1/auth/email`, and only
  ever marked proved by a link followed from that address.

So the rollout for existing people is: sign in, `PUT /api/v1/auth/email`,
follow the link. Until they do, `POST /api/v1/admin/users/:login/reset-password`
stays their way back in, and an operator may want to say so on the sign-in
page.

Rollback is in the migration header: stop the server, drop
`account_email_tokens` and `account_emails`, delete version 17 from
`shift_schema_migrations`. Take a copy first
(`sqlite3 shift.sqlite '.backup account-emails.sqlite'`) — a proved address
exists nowhere else, and without it every person proves their address again.
