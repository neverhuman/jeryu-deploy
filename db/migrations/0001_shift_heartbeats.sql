-- 0001_shift_heartbeats: todoq slot heartbeats for the jeryu Shift pages.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/shift/heartbeats.rs),
-- applied at startup by that module's migration runner, which records each
-- migration's sha256 in shift_schema_migrations and refuses to start if an
-- applied migration's file has changed. Migrations here are immutable once
-- released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server and delete shift.sqlite. The table holds
-- telemetry only (14-day retention, pruned by the server), so nothing of
-- record is lost; todoq keeps sending heartbeats and the history refills.
-- Backfill: none. Lock safety: a new file and a new table; no existing data is
-- touched and creation takes milliseconds. Overlapping binaries: an older
-- binary never opens shift.sqlite. No foreign keys (single table) and no
-- tenant-scoped rows (operator telemetry), so no row level security applies.
-- The CHECK constraint keeps state within the contract's four values.

CREATE TABLE IF NOT EXISTS shift_heartbeats (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    received_ms   INTEGER NOT NULL,
    reporter      TEXT    NOT NULL,
    operator      TEXT    NOT NULL,
    host          TEXT    NOT NULL,
    slot          TEXT    NOT NULL,
    family        TEXT    NOT NULL,
    state         TEXT    NOT NULL CHECK (state IN ('idle', 'working', 'stopping', 'paused')),
    todo_id       TEXT,
    stage         TEXT,
    lease_until   TEXT,
    shift         TEXT,
    planned_slots INTEGER,
    schedule_json TEXT,
    version       TEXT
);

CREATE INDEX IF NOT EXISTS shift_heartbeats_received
    ON shift_heartbeats (received_ms);

CREATE INDEX IF NOT EXISTS shift_heartbeats_slot
    ON shift_heartbeats (operator, host, slot, family, received_ms);
