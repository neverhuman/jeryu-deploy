-- 0003_jankurai_disputes: operator disputes filed against a jankurai finding,
-- behind POST/GET /api/v1/jankurai/disputes and the dispute counts in
-- GET /api/v1/jankurai/overview.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/jankurai/disputes.rs),
-- applied at startup by the shift migration runner (web/shift/heartbeats.rs),
-- which records each migration's sha256 in shift_schema_migrations and refuses
-- to start if an applied migration's file has changed. Migrations here are
-- immutable once released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE jankurai_disputes` and delete
-- version 3 from shift_schema_migrations. A dispute is operator opinion about
-- a score the forge store already holds, so dropping it loses no audit record.
-- Backfill: none; the table starts empty and only admins append to it.
-- Lock safety: a new table and new indexes in a telemetry-only file; no
-- existing row is touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table, and
-- the runner only checks the versions it knows, so old and new can overlap.
-- No foreign keys: score_id names a jankurai score held in the forge core's
-- own store (a different file), and a dispute must outlive the per-branch
-- score retention that may drop that row. score_id is therefore validated by
-- the route, not by the database. The CHECK constraints keep the two columns
-- a route cannot re-derive non-empty. No tenant-scoped rows: filing is
-- admin-only and reads are per-score for any logged-in user, so no row level
-- security applies. The UNIQUE index makes filing idempotent per
-- (score, rule, path, line, author): a retried POST returns the first row
-- instead of inflating the dispute rate the overview reports.

CREATE TABLE IF NOT EXISTS jankurai_disputes (
    id          TEXT    PRIMARY KEY,
    created_ms  INTEGER NOT NULL,
    score_id    TEXT    NOT NULL CHECK (length(score_id) > 0),
    repo        TEXT    NOT NULL,
    commit_sha  TEXT    NOT NULL,
    rule_id     TEXT    NOT NULL CHECK (length(rule_id) > 0),
    path        TEXT,
    line        INTEGER,
    reason      TEXT    NOT NULL CHECK (length(reason) > 0),
    author      TEXT    NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS jankurai_disputes_identity
    ON jankurai_disputes (score_id, rule_id, ifnull(path, ''), ifnull(line, -1), author);

CREATE INDEX IF NOT EXISTS jankurai_disputes_score
    ON jankurai_disputes (score_id, created_ms);

CREATE INDEX IF NOT EXISTS jankurai_disputes_rule
    ON jankurai_disputes (rule_id, created_ms);
