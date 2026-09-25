-- 0004_work_bridge_repairs: the pending Work-mirror repair queue, recorded when
-- a GitHub-compatible issue write cannot reach the Work Tracker store.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api
-- (github/work_bridge_repairs.rs), applied at startup by the shift migration
-- runner (web/shift/heartbeats.rs), which records each migration's sha256 in
-- shift_schema_migrations and refuses to start if an applied migration's file
-- has changed. Migrations here are immutable once released; numbering is a
-- zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE work_bridge_repairs` and delete
-- version 4 from shift_schema_migrations. The queue then falls back to the
-- in-process one it had before, so a rollback loses only the repairs still
-- pending at that moment, not any issue or Work row.
-- Backfill: none; the table starts empty and fills as bridge writes fail.
-- Lock safety: a new table in a telemetry-only file; no existing row is
-- touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table, and
-- the runner only checks the versions it knows, so old and new can overlap.
-- No foreign keys: the owner/repo/issue_number triple names an issue held in
-- the forge core's own store (a different file), and a repair must outlive an
-- issue the core may drop; the route validates the triple instead. The primary
-- key is the repair's identity, so a bridge write that keeps failing refreshes
-- one row instead of growing the queue without bound, and a later bridge write
-- that succeeds for that issue deletes its rows.

CREATE TABLE IF NOT EXISTS work_bridge_repairs (
    owner         TEXT    NOT NULL CHECK (length(owner) > 0),
    repo          TEXT    NOT NULL CHECK (length(repo) > 0),
    issue_number  INTEGER NOT NULL,
    operation     TEXT    NOT NULL CHECK (length(operation) > 0),
    code          TEXT    NOT NULL CHECK (length(code) > 0),
    recorded_ms   INTEGER NOT NULL,
    work_key      TEXT,
    reason        TEXT    NOT NULL,
    common_fixes  TEXT    NOT NULL,
    docs_url      TEXT    NOT NULL,
    repair_hint   TEXT    NOT NULL,
    PRIMARY KEY (owner, repo, issue_number, operation, code)
);

CREATE INDEX IF NOT EXISTS work_bridge_repairs_recorded
    ON work_bridge_repairs (recorded_ms);
