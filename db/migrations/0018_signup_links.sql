-- 0018_signup_links: the single-use links that prove an address before an
-- account exists, and the marker that a temporary password was mailed.
--
-- Sign-up used to create an account from a login and a password alone, so an
-- account could exist with no address and no way to recover it. Sign-up now
-- starts from an address: POST /api/v1/auth/signup/start mails a link to it,
-- and POST /api/v1/auth/signup creates the account only by spending that
-- link, storing the address already proved. No account row exists yet when
-- the link is issued, so these rows name an address, not a login, which is
-- why they live outside account_email_tokens.
--
-- signup_links is one row per message sent to an address that asked to sign
-- up. `kind` = 'link' is a spendable link; `kind` = 'notice' records the
-- "an account already uses this address" message, which carries no link and
-- is written already consumed. Both count towards the durable per-address
-- send cap, so the cap is reached at the same rate whether or not an account
-- holds the address, and a 429 says nothing about who has an account here.
-- The link's secret is never stored: `token_hash` is its lowercase hex
-- sha256, which is also the primary key. A row is spendable only while
-- `kind = 'link' AND consumed_ms IS NULL AND expires_ms > now`; spending it
-- writes `consumed_ms` in the same statement that selects it, so a replay
-- finds nothing. Issuing a new link retires the address's older outstanding
-- ones, and rows past their expiry are pruned on write.
--
-- account_emails.reset_mailed_ms is set when an administrator's reset mailed a
-- temporary password to the account's stored address, and cleared when the
-- address changes. Signing in with that password and choosing a new one
-- proves the person reads that mailbox, so the forced password change marks
-- the address verified and clears the marker. A NULL marker verifies nothing.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/signup.rs and
-- web/admin_reset.rs, on web/account_email.rs's store), applied at startup by
-- the shift migration runner (web/shift/heartbeats.rs), which records each
-- migration's sha256 in shift_schema_migrations and refuses to start if an
-- applied migration's file has changed. Migrations here are immutable once
-- released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE signup_links`, delete version
-- 18 from shift_schema_migrations, and either leave reset_mailed_ms in place
-- (an older binary never reads it) or rebuild account_emails without it on a
-- SQLite that predates DROP COLUMN. Accounts, passwords and proved addresses
-- are untouched; the only loss is the sign-up links in flight, whose holders
-- ask for a new one. An older binary serves the old login-and-password
-- sign-up again, so roll back only together with the binary.
-- Backfill: none. The table starts empty and the new column starts NULL, so
-- no outstanding admin reset proves anything retroactively: only a temporary
-- password mailed by this binary does.
-- Lock safety: one new table and its indexes plus one nullable column added
-- without a default; SQLite rewrites no row for either, so it takes
-- milliseconds whatever the size of account_emails.
-- Overlapping binaries: an older binary never reads signup_links or
-- reset_mailed_ms; while both run, the older one still accepts a bare
-- login-and-password sign-up, which is the behaviour this migration's binary
-- retires. The runner only checks the versions it knows, so old and new can
-- overlap.
-- No foreign keys: an address is plain text, folded to lowercase by the
-- writer exactly as account_emails folds it. Rows are readable only by the
-- server process and are never served to a caller.

CREATE TABLE IF NOT EXISTS signup_links (
    -- Lowercase hex sha256 of the link secret. The secret itself is in the
    -- email and nowhere else.
    token_hash    TEXT    PRIMARY KEY CHECK (length(token_hash) = 64),
    kind          TEXT    NOT NULL CHECK (kind IN ('link', 'notice')),
    -- Folded to lowercase by the writer.
    address       TEXT    NOT NULL CHECK (length(address) > 0),
    issued_ms     INTEGER NOT NULL,
    expires_ms    INTEGER NOT NULL,
    -- NULL while the link is still spendable; set when it is spent or retired.
    consumed_ms   INTEGER
);

-- How many messages one address was sent inside a window, which is the
-- durable half of the send cap: the per-address limiter is in memory and a
-- restart would forget it.
CREATE INDEX IF NOT EXISTS signup_links_address
    ON signup_links (address, issued_ms);

-- The expiry prune.
CREATE INDEX IF NOT EXISTS signup_links_expires
    ON signup_links (expires_ms);

-- When an administrator's reset mailed a temporary password to `address`;
-- NULL otherwise.
ALTER TABLE account_emails ADD COLUMN reset_mailed_ms INTEGER;
