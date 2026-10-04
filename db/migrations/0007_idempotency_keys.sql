-- 0007_idempotency_keys: kept answers to POST writes that carried an
-- `Idempotency-Key`. One row per (caller, key) scope digest, holding the
-- request's fingerprint and — once the handler has answered — the status,
-- headers and body to replay. A row with `status IS NULL` is a request that
-- is still running, so a repeat answers 409 idempotency_key_in_flight.
--
-- The store was in memory, so a retry that arrived after a restart or a
-- deploy ran the write a second time and filed the work twice. On disk the
-- keys outlive the process and the retry replays the first answer.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/idempotency.rs),
-- applied at startup by the shift migration runner (web/shift/heartbeats.rs),
-- which records each migration's sha256 in shift_schema_migrations and
-- refuses to start if an applied migration's file has changed. Migrations
-- here are immutable once released; numbering is a zero-padded 4-digit
-- sequence.
--
-- Rollback: stop the server, then `DROP TABLE idempotency_keys` and delete
-- version 7 from shift_schema_migrations. Every kept answer falls away, so a
-- retry in the next day runs its write again; nothing else is touched.
-- Backfill: none; the table starts empty and fills as writes carry keys.
-- Lock safety: a new table in a telemetry-only file; no existing row is
-- touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table and
-- keeps its answers in memory, so a retry it serves may run twice; the runner
-- only checks the versions it knows, so old and new can overlap.
-- No foreign keys: a scope is a digest of credentials and a client's key, not
-- a stored row. Rows are dropped once they are a day old, and each row is
-- readable only by the caller whose credentials hash into its scope.

CREATE TABLE IF NOT EXISTS idempotency_keys (
    scope       TEXT    PRIMARY KEY CHECK (length(scope) > 0),
    fingerprint BLOB    NOT NULL,
    at_ms       INTEGER NOT NULL,
    -- NULL while the request is still running.
    status      INTEGER,
    -- The kept answer's headers, a JSON array of [name, value] pairs.
    headers     TEXT,
    body        BLOB
);

CREATE INDEX IF NOT EXISTS idempotency_keys_at ON idempotency_keys (at_ms);
