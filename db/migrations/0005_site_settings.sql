-- 0005_site_settings: instance-wide settings an administrator sets from the
-- web UI, behind GET /api/v1/site-settings and GET/PUT
-- /api/v1/admin/site-settings. The first key is `internal_wiki`, the id of the
-- repository the left navigation shows as the instance's wiki.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/site_settings.rs),
-- applied at startup by the shift migration runner (web/shift/heartbeats.rs),
-- which records each migration's sha256 in shift_schema_migrations and refuses
-- to start if an applied migration's file has changed. Migrations here are
-- immutable once released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE site_settings` and delete
-- version 5 from shift_schema_migrations. Every key falls back to unset, so
-- the wiki link disappears until an administrator picks the repository again;
-- no repository or account row is touched.
-- Backfill: none; the table starts empty and only admins write to it.
-- Lock safety: a new table in a telemetry-only file; no existing row is
-- touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table, and
-- the runner only checks the versions it knows, so old and new can overlap.
-- No foreign keys: a value such as a repository id names a row held in the
-- forge core's own store (a different file); the route validates it on write
-- and resolves it on every read, so a deleted repository reads as unset. One
-- row per key, written by an admin, read by any caller; no tenant-scoped rows,
-- so no row level security applies.

CREATE TABLE IF NOT EXISTS site_settings (
    key         TEXT    PRIMARY KEY CHECK (length(key) > 0),
    value       TEXT    NOT NULL,
    updated_ms  INTEGER NOT NULL,
    updated_by  TEXT    NOT NULL
);
