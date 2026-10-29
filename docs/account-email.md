# Account email, sign-up, verification and password recovery

An account's password lives in the forge core. Its email address, whether that
address has been proved, and the single-use links that prove it or recover the
password live in `<data_dir>/shift.sqlite`
(`db/migrations/0017_account_emails.sql`), owned by
`crates/jeryu-api/src/web/account_email.rs`. Sign-up links and the
mailed-reset marker are `db/migrations/0018_signup_links.sql`, owned by
`crates/jeryu-api/src/web/signup.rs` and
`crates/jeryu-api/src/web/admin_reset.rs`.

Before this surface existed an account carried no address at all, so somebody
who lost their password waited for an administrator to run
`POST /api/v1/admin/users/:login/reset-password`. That route now mails the
temporary password to the account's address (see
[Administrator reset](#administrator-reset)).

## Routes

| Route | Credential | Answer |
|---|---|---|
| `GET /api/v1/auth/email` | the account | `{"address": …\|null, "verified": bool}` |
| `PUT /api/v1/auth/email` | the account | `202 {"result":"sent","verified":false}` |
| `POST /api/v1/auth/email/verify/resend` | the account | `202 {"result":"sent","verified":false}` |
| `POST /api/v1/auth/email/verify` | none | `200 {"result":"verified","verified":true}` |
| `POST /api/v1/auth/password/forgot` | none | `202 {"result":"sent"}` |
| `POST /api/v1/auth/password/reset` | none | `200 {"result":"reset"}` |
| `GET /api/v1/auth/signup/policy` | none | `{"mode":"closed"\|"email","waitlistUrl":…\|null}` |
| `POST /api/v1/auth/signup/start` | none | `202 {"result":"sent"}`; `403 signup_closed` unless `JERYU_SIGNUP=email` |
| `POST /api/v1/auth/signup` | none | the account, its CSRF token and the session cookie; `403 signup_closed` unless `JERYU_SIGNUP=email` |
| `POST /api/v1/admin/users/:login/reset-password` | an administrator | `202 {"result":"sent","login":…,"email":…}` |

The link routes need no credential because a mail client reaches them: a
link is followed from a mailbox that holds no session, and the link itself
names the account. The sign-up routes need none because no account exists yet.

## Sign-up

**Sign-up is off unless `JERYU_SIGNUP=email`.** By default (unset, empty,
`closed`, or any value this binary does not know) both sign-up routes answer
`403 signup_closed`, "sign-up is by invitation; join the waitlist", with a
`waitlistUrl` field, before anything else, so no message is ever sent.
Accounts are then made by an administrator (`POST /api/v1/admin/users`, then
the mailed reset below), and people join a waitlist: `JERYU_WAITLIST_URL`, an
absolute `https` URL on another site, or, without one, the forge's own
`POST /api/v1/waitlist` (see [Waitlist receipt](#waitlist-receipt)). An
unknown `JERYU_SIGNUP` value or an invalid
`JERYU_WAITLIST_URL` is named on one startup line starting
`jeryu-api sign-up:` and ignored.

`GET /api/v1/auth/signup/policy` (open) answers
`{"mode":"closed"|"email","waitlistUrl":…|null}`, so the web app draws the
right surface before anybody signs in.

With `JERYU_SIGNUP=email`, an account is only ever created with its address
already proved:

1. `POST /api/v1/auth/signup/start` `{"email": "…"}` mails a link,
   `<base>/create-account?token=…`, good for a day. Every valid address
   answers `202 {"result":"sent"}`. An address an account has already proved
   is mailed a note naming that account's login and the
   `<base>/forgot-password` page instead of a link, so the answer never says
   whether an account holds the address; the mailbox does, and only to its
   owner.
2. `POST /api/v1/auth/signup` `{"token": "…", "login": "…", "password": "…"}`
   spends the link, creates the account, stores the address proved, and
   answers as sign-in does.

Refusals:

| Answer | When |
|---|---|
| `403 signup_closed` | sign-up is closed (the default); the body carries `waitlistUrl` |
| `503 mail_unavailable` | the forge cannot send mail; sign-up is impossible without it, so `start` says so for every address |
| `422 invalid_input` | `start` got a malformed address, or `signup` a password under 12 bytes (checked before the link is spent) |
| `422 email_required` | `signup` got no token: verify an address with `start` first |
| `422 signup_link_invalid` | the link is expired, already spent, or never issued |
| `409 conflict` | the login is taken (the link stays spendable, so pick another), or another account proved the address meanwhile |

A login core refuses (`422 invalid_input`) also leaves the link spendable.
Another account's *unproved* claim on the address gives way to the sign-up
that proved it. If another account proves the address in the instant between
creating the account and storing its address, the new account is disabled and
the answer is `409`: an account never exists without the address it proved.

Administrator-created accounts (`POST /api/v1/admin/users`) are unchanged and
start with no address.

## Waitlist receipt

A join that lists a new address (`POST /api/v1/waitlist`, owned by
`crates/jeryu-api/src/web/waitlist_mail.rs`) mails that address one fixed,
branded receipt, "You're on the JeRyu waitlist", through the same
`JERYU_MAIL_*` transport. The route's answer never depends on it: the send runs after the
response, so a new and a repeat address still get identical bytes.

No receipt goes out when:

- the address is already listed (a repeat join);
- another listed address reaches the same mailbox: the same local part
  without its `+tag`, and for `gmail.com`/`googlemail.com` without dots;
- more than 60 new addresses joined in the last hour, across the whole list;
- mail is not configured.

The message never carries the name or note the visitor typed, and the
recipient passes the same address check as every other message.

## Waitlist forwarding

An operator who keeps one list across several products can have the forge
send each **newly listed** address on, after the visitor has their answer
(`crates/jeryu-api/src/web/waitlist_forward.rs`). The forge keeps its own row
either way; a repeat join sends nothing.

| Variable | Meaning |
|---|---|
| `JERYU_WAITLIST_FORWARD_URL` | an `https` URL (or `http` to loopback) that takes one JSON `POST` per new address |
| `JERYU_WAITLIST_FORWARD_SECRET_FILE` | a file holding the shared secret (32+ characters) |
| `JERYU_WAITLIST_PRODUCT` | the product slug sent along, default `jeryu` |

The request:

- headers `X-Waitlist-Timestamp: <unix seconds>` and
  `X-Waitlist-Signature: sha256=<hex>`, the HMAC-SHA256 of
  `"<timestamp>.<raw body>"` under the secret; a receiver should compare in
  constant time and refuse a timestamp more than a few minutes old;
- body `{"product","email","name","note","joinedAt","origin"}`, where
  `origin` is `JERYU_MAIL_BASE_URL` (or `null`).

Any 2xx is success. A failure is retried twice (after 2s and 10s), then
left. Each outcome is audited under subject `waitlist_forward` as
`waitlist.forwarded` (`completed` with the attempt count, or `failed` with
the reason), never with the address or the URL. A missing or invalid
setting turns forwarding off and is named on one startup line starting
`jeryu-api waitlist:`.

## Administrator reset

`POST /api/v1/admin/users/:login/reset-password`, body `{}` or
`{"email": "…"}`, sets a one-time password, drops the account's sessions,
tokens and bots, and forces a password change at the next sign-in. Then:

- **Mail configured, message sent:** the login, the temporary password and
  the sign-in address (`<base>/`) are mailed to the account, and the answer
  is `202 {"result":"sent","login":…,"email":…}`, without the password.
- **Mail configured, send failed:** the reset stands and the administrator
  is shown the password to hand over:
  `200 {"result":"shown","login":…,"password":…,"email":…,"mailError":"could not send the message"}`.
- **Mail not configured** (the default): the reset happens and the answer is
  `200 {"result":"shown","login":…,"password":…,"email":…|null}`, so an
  account without mail still has a way back in.

A shown password is a secret in the answer: an idempotent replay of it gets
the placeholder, never the password again.

The address is the account's proved one if it has one; otherwise the
supplied `email`, which is stored unproved; otherwise the stored unproved
address. Without mail an address is optional.

| Answer | When |
|---|---|
| `422 email_required` | mail is configured, the account has no address and none was supplied; nothing was changed |
| `422 invalid_input` | the supplied address is malformed |
| `409 conflict` | the account has a proved address and a different one was supplied, or another account holds the supplied one |

Signing in with the mailed password and completing the forced change proves
the person reads that mailbox, so `POST /api/v1/auth/password` then marks the
address verified. Only a reset whose message left records that marker
(`account_emails.reset_mailed_ms`); a shown password records none and clears
an older one, and claiming another address clears it too.

`GET /api/v1/admin/users` reports each account's `email` (or `null`) and
`emailVerified`.

`POST /api/v1/auth/password/forgot` answers `202 {"result":"sent"}` for every
well-formed request, byte for byte. Whether an account holds the address,
whether it proved it, whether the hourly cap was already spent and whether a
message actually left are all invisible to the caller and recorded in the
audit log instead. A malformed address is the same answer, because refusing it
would tell a caller which of its guesses this forge would even store.

The answer carries no timing of any of that either. Once the rate limits are
checked, the handler spawns the look-up, the link and the hand-over to the
host's mailer and answers at once (`crates/jeryu-api/src/web/background.rs`),
so an address nobody has proved costs a caller the same wait as one that just
received a link. An operator reads the outcome in the audit log, never in the
response.

An unspendable link — expired, already spent, never issued, or offered for the
wrong purpose — is one refusal: `422 reset_link_invalid` or
`422 verification_link_invalid`.

## What keeps a link safe

- The secret is 32 bytes of operating-system randomness. Only its sha256 is
  stored, so the database holds nothing spendable and neither does a backup.
- Spending is one `UPDATE … WHERE consumed_ms IS NULL AND expires_ms > now`,
  so a replay finds nothing.
- A verification link lasts a day; a reset link lasts an hour.
- An unproved address claim reserves the address for one day. A later claim
  can retire an expired unproved reservation and all its outstanding links
  atomically. Resending a verification link does not extend the reservation.
  A proved address never expires for another account. A recent reservation
  or proved owner still gives an authenticated claimant `409`; password
  recovery continues to answer identically for known and unknown addresses.
- A reset link is only ever sent to a **proved** address, so taking over an
  unproved address takes over no account. It is never in a response body:
  only the message carries it.
- The answer to a reset request is sent before the message is, so its timing
  reveals nothing about the address.
- Claiming an address, proving one, or finishing a reset retires that
  account's other outstanding links.
- The secret in a reset link is the recovery secret core issues
  (`ForgeCore::issue_password_recovery`). The forge binds it to the proved
  address it was mailed to, and core spends it (`recover_password`).
- A finished reset is core's self-service recovery: it rotates the account's
  auth epoch, ends every browser session, deletes every personal access token,
  revokes the account's own bots' keys, refresh tokens and agent tokens, and
  clears a forced password change. Recovering therefore signs the account out
  everywhere. Team bots of the account's organizations are untouched, so an
  agent that must survive a recovery should use a team bot token.
- A password below core's floor or above 1024 bytes, and a link secret above
  128 bytes, are refused *before* the link is spent, so a rejected request
  does not burn the person's one link.
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
| `POST …/signup/start` per client address | 5 / hour |
| `POST …/signup/start` per address asked for | 3 / hour |
| `POST …/signup/start` for the whole route | 30 / minute |
| `POST …/signup` per client address and login | the sign-in cap |
| sign-up messages sent per address | 5 / hour (in the store) |
| reset links issued per account | 5 / hour (in the store) |
| verification links issued per account | 5 / hour (in the store) |

The per-route buckets are in memory and a restart forgets them; the three
"in the store" caps are counted from `account_email_tokens` and
`signup_links`, so a restart loop cannot be used to send somebody a hundred
messages. The sign-up message cap counts the "already have an account" note
as well as links, so it is reached at the same rate either way. Per-address
caps hold even when every request arrives through one proxy address.

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
| `JERYU_MAIL_REPLY_TO` | optional: a plain address replies go to, e.g. `help@example.com` (sent as `Reply-To`, and named in a branded message's footer) |
| `JERYU_MAIL_FOOTER` | optional: one line closing a branded message, e.g. `Run by Example Co.` |
| `JERYU_MAIL_FOOTER_URL` | optional: an `https` link for that line |

A branded message (today, the waitlist receipt) is `multipart/alternative`:
the same words as text, then HTML in base64, laid out by
`crates/jeryu-api/src/web/mail_layout.rs`. Its wordmark is the web app's
`/email/jeryu-wordmark.png` on `JERYU_MAIL_BASE_URL`. A sendmail program that
only takes text still delivers it; a bridge to an HTTP mail API should send
both parts and the `Reply-To`. An invalid optional value is named on one
startup line starting `jeryu-api mail:` and ignored.

`JERYU_MAIL_SENDMAIL` wins when both transports are set. Its configured arguments
come first, then `--`, then the recipient, so an address beginning with `-`
cannot become an MTA option. A spooled message is
written to `<name>.tmp` and renamed, so a path unit or cron job that submits
the directory never reads half a message. Retries belong to the program or the
job, not to this process.

`JERYU_MAIL_BASE_URL` is not guessed from the listener: a link that points at
the bind address is a link nobody can follow. The links built on it are
`<base>/reset-password?token=…`, `<base>/verify-email?token=…`,
`<base>/create-account?token=…`, `<base>/forgot-password` and `<base>/` (sign
in), which the web app serves.

**All three pieces are needed** — a sender, a base URL, and one transport.
Without them sign-up start answers `503 mail_unavailable` and the
administrator reset shows the one-time password instead of mailing it; the
recovery routes
send nothing and answer exactly as they would have, and the unsent message is
recorded as a `failed`
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
| `account.signup.requested` | a sign-up link or "already have an account" note was sent, or was not |
| `account.signup.completed` | a sign-up link was spent and the account created (`failed`: the account was disabled) |
| `account.password_reset.admin_mailed` | an administrator's reset was mailed (`failed`: the marker could not be recorded) |
| `account.password_reset.admin_shown` | an administrator's reset was shown instead; the detail says why (no mail, or the send failed) |
| `account.email.claim_released` | against an account whose unproved address a new sign-up proved and took |

The waitlist receipt is recorded under its own subject, `waitlist_mail`, as
`waitlist.confirmation`: `completed` when sent, `requested` with a reason when
held back (repeat mailbox, hourly ceiling, no mail configured), `failed` when
the address was refused or the send failed.

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

`0018_signup_links` adds `signup_links` and the nullable
`account_emails.reset_mailed_ms`, and backfills nothing: no outstanding
administrator reset proves an address retroactively. Its rollback is in its
header and goes together with the binary, since an older binary serves the old
login-and-password sign-up.
