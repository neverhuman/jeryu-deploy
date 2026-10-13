-- 0017_account_emails: the one email address an account owns, whether that
-- address has been proved, and the single-use links that prove it or recover
-- the password.
--
-- An account had no address at all, so a person who lost their password had
-- no way back in but an administrator running
-- POST /api/v1/admin/users/:login/reset-password for them. These two tables
-- are what a self-service recovery needs, and nothing more.
--
-- account_emails holds one row per account: the address, and `verified_ms`,
-- which is NULL until a verification link is followed. Unverified is
-- therefore the stored default and never an absent row read as verified;
-- recovery reads only rows with `verified_ms IS NOT NULL`.
--
-- An address is folded to lowercase on the way in, the same way the core
-- folds a waitlist address, so one address is one address across the forge
-- and the unique index below is all it takes to keep two accounts from
-- claiming one. The folded form is also what is served back.
--
-- account_email_tokens is one row per issued link. The link's secret is never
-- stored: `token_hash` is its lowercase hex sha256, which is also the primary
-- key, so a lookup is by hash and a stolen database yields no usable link.
-- A row is spendable only while `consumed_ms IS NULL AND expires_ms > now`;
-- spending it writes `consumed_ms` in the same statement that selects it, so
-- a replay finds nothing. Changing or re-verifying an address retires that
-- account's outstanding rows: `consumed_ms` is set rather than the row
-- dropped, so a retired link still counts towards the hourly send cap. Rows
-- past their expiry are pruned on write.
-- The token keeps the `address` it was issued for, so a link issued for one
-- address does not verify a different one the account moved to afterwards.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api
-- (web/account_email.rs), applied at startup by the shift migration runner
-- (web/shift/heartbeats.rs), which records each migration's sha256 in
-- shift_schema_migrations and refuses to start if an applied migration's file
-- has changed. Migrations here are immutable once released; numbering is a
-- zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE account_email_tokens`,
-- `DROP TABLE account_emails` and delete version 17 from
-- shift_schema_migrations. Sign-in, sessions and tokens are untouched by
-- that: the password lives in the forge core, not here, so the only loss is
-- the recovery and verification surface, which answers as though no account
-- has an address. Take a copy first (sqlite3 shift.sqlite '.backup
-- account-emails.sqlite'): a verified address exists nowhere else, and
-- without the copy every person has to prove their address again.
-- Backfill: none, and deliberately none. The table starts empty, so every
-- account that already existed reads as having no address: it cannot start a
-- recovery, and nothing about its sign-in changes. An address is only ever
-- written by the account's own signed-in request, and only ever marked
-- verified by a link followed from that address, so no backfill could put a
-- trustworthy address here. Existing people set one from the account page;
-- until they do, the administrator reset stays their way back in.
-- Lock safety: two new tables and their indexes; creation takes milliseconds
-- and touches no existing row.
-- Overlapping binaries: an older binary never reads or writes these tables
-- and serves no recovery routes, so a request it takes answers 404 while a
-- newer one answers; the runner only checks the versions it knows, so old and
-- new can overlap.
-- No foreign keys: `login` is plain text, as elsewhere in this file, so a row
-- outlives the account it names (and a deleted login's rows are swept by the
-- expiry prune). Rows are readable only by the server process; no row is
-- served to a caller other than the account it belongs to.

CREATE TABLE IF NOT EXISTS account_emails (
    login       TEXT    PRIMARY KEY CHECK (length(login) > 0),
    -- Folded to lowercase by the writer.
    address     TEXT    NOT NULL CHECK (length(address) > 0),
    -- NULL until a verification link for `address` is followed.
    verified_ms INTEGER,
    added_ms    INTEGER NOT NULL
);

-- One account per address, so a recovery lookup by address is unambiguous.
CREATE UNIQUE INDEX IF NOT EXISTS account_emails_address
    ON account_emails (address);

CREATE TABLE IF NOT EXISTS account_email_tokens (
    -- Lowercase hex sha256 of the link secret. The secret itself is in the
    -- email and nowhere else.
    token_hash    TEXT    PRIMARY KEY CHECK (length(token_hash) = 64),
    purpose       TEXT    NOT NULL CHECK (purpose IN ('verify', 'reset')),
    login         TEXT    NOT NULL CHECK (length(login) > 0),
    address       TEXT    NOT NULL CHECK (length(address) > 0),
    issued_ms     INTEGER NOT NULL,
    expires_ms    INTEGER NOT NULL,
    -- NULL while the link is still spendable; set when it is spent.
    consumed_ms   INTEGER
);

-- How many links an account was issued for one purpose inside a window, which
-- is the durable half of the send cap: the per-address limiter is in memory
-- and a restart would forget it.
CREATE INDEX IF NOT EXISTS account_email_tokens_login
    ON account_email_tokens (login, purpose, issued_ms);

-- The expiry prune.
CREATE INDEX IF NOT EXISTS account_email_tokens_expires
    ON account_email_tokens (expires_ms);
