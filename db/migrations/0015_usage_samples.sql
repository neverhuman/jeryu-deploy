-- 0015_usage_samples: live token usage per worker slot, behind
-- GET /api/v1/usage, the Activity page's token chart and the `usage` object on
-- GET /api/v1/shift/workers. One row per report: the running totals a slot's
-- heartbeat (or a todo.attempt_finished event) carried for the attempt it was
-- working at the time. Totals are cumulative within one (slot, todo_id), so a
-- rate is the delta between two consecutive samples of the same pair and a
-- restart, a reset or a new attempt never double counts.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/usage.rs), applied
-- at startup by the shift migration runner (web/shift/heartbeats.rs), which
-- records each migration's sha256 in shift_schema_migrations and refuses to
-- start if an applied migration's file has changed. Migrations here are
-- immutable once released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE usage_samples` and delete
-- version 15 from shift_schema_migrations. The token chart loses its history
-- and live usage reads as null until producers report again; todo files, pull
-- requests and the event log stay the record, so nothing of record is lost.
-- Backfill: none; producers start reporting once the binary is live.
-- Lock safety: a new table and new indexes in a telemetry-only file; no
-- existing row is touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table, and
-- the runner only checks the versions it knows, so old and new can overlap.
-- No foreign keys (todo_id names a todo file, not a stored row, and a sample
-- outlives the attempt it measured) and no tenant-scoped rows: reads are
-- admin-only, so no row level security applies. Rows are pruned by the server
-- with the same 30-day retention as the pipeline event log.

CREATE TABLE IF NOT EXISTS usage_samples (
    id                          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms                       INTEGER NOT NULL,
    operator                    TEXT    NOT NULL,
    host                        TEXT    NOT NULL,
    slot                        TEXT    NOT NULL,
    family                      TEXT    NOT NULL,
    todo_id                     TEXT,
    model                       TEXT,
    input_tokens                INTEGER NOT NULL DEFAULT 0,
    output_tokens               INTEGER NOT NULL DEFAULT 0,
    cache_read_input_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd                    REAL,
    turns                       INTEGER,
    updated_at                  TEXT
);

CREATE INDEX IF NOT EXISTS usage_samples_ts
    ON usage_samples (ts_ms);

CREATE INDEX IF NOT EXISTS usage_samples_slot
    ON usage_samples (operator, host, slot, family, todo_id, ts_ms);
