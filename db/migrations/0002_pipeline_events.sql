-- 0002_pipeline_events: the append-only pipeline event log behind
-- GET/POST /api/v1/events, the Activity page and the attention inbox.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/pipeline/store.rs),
-- applied at startup by the shift migration runner (web/shift/heartbeats.rs),
-- which records each migration's sha256 in shift_schema_migrations and refuses
-- to start if an applied migration's file has changed. Migrations here are
-- immutable once released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE pipeline_events` and delete
-- version 2 from shift_schema_migrations (or delete shift.sqlite, which also
-- clears heartbeat telemetry). The table holds operator telemetry only (30-day
-- retention, pruned by the server); todo files, pull requests, statuses and
-- deployments stay the record, so nothing of record is lost.
-- Backfill: none; producers start appending once the binary is live.
-- Lock safety: a new table and new indexes in a telemetry-only file; no
-- existing row is touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table, and
-- the runner only checks the versions it knows, so old and new can overlap.
-- No foreign keys (join keys are plain text: an event outlives the todo or
-- pull request it names) and no tenant-scoped rows: reads are admin-only, so
-- no row level security applies. seq is AUTOINCREMENT so a pruned sequence
-- number is never reused and a client cursor stays valid.
-- The CHECK constraint keeps needs_human a boolean.

CREATE TABLE IF NOT EXISTS pipeline_events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms       INTEGER NOT NULL,
    source      TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    reporter    TEXT    NOT NULL,
    actor       TEXT,
    family      TEXT,
    repo        TEXT,
    pr          INTEGER,
    sha         TEXT,
    todo_id     TEXT,
    shift       TEXT,
    outcome     TEXT,
    needs_human INTEGER NOT NULL DEFAULT 0 CHECK (needs_human IN (0, 1)),
    summary     TEXT    NOT NULL,
    reason      TEXT,
    cost_usd    REAL,
    seconds     INTEGER,
    log_tail    TEXT,
    log_url     TEXT,
    detail_json TEXT
);

CREATE INDEX IF NOT EXISTS pipeline_events_ts
    ON pipeline_events (ts_ms);

CREATE INDEX IF NOT EXISTS pipeline_events_kind
    ON pipeline_events (kind, seq);

CREATE INDEX IF NOT EXISTS pipeline_events_todo
    ON pipeline_events (todo_id, seq);

CREATE INDEX IF NOT EXISTS pipeline_events_repo_pr
    ON pipeline_events (repo, pr, seq);

CREATE INDEX IF NOT EXISTS pipeline_events_family
    ON pipeline_events (family, seq);
