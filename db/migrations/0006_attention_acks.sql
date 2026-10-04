-- 0006_attention_acks: deliberately deferred attention items, behind
-- GET/POST /api/v1/attention/acks. One row per attention item id (any kind:
-- a todo, a failing mirror, a draft pull request), holding the time the item
-- comes back. While that time is in the future the inbox leaves the item out,
-- so a known problem somebody has decided to live with stops asking.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api
-- (web/pipeline/attention/acks.rs), applied at startup by the shift migration
-- runner (web/shift/heartbeats.rs), which records each migration's sha256 in
-- shift_schema_migrations and refuses to start if an applied migration's file
-- has changed. Migrations here are immutable once released; numbering is a
-- zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE attention_acks` and delete
-- version 6 from shift_schema_migrations. Every acknowledgement falls away and
-- the items it hid are listed again; nothing else is touched.
-- Backfill: none; the table starts empty and only admins write to it.
-- Lock safety: a new table in a telemetry-only file; no existing row is
-- touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table, and
-- the runner only checks the versions it knows, so old and new can overlap.
-- No foreign keys: item_id names an item the inbox computes on each call, not
-- a stored row, and an id whose cause is gone simply never matches again. One
-- row per item, written and read by admins only, so no row level security
-- applies.

CREATE TABLE IF NOT EXISTS attention_acks (
    item_id   TEXT    PRIMARY KEY CHECK (length(item_id) > 0),
    until_ms  INTEGER NOT NULL,
    note      TEXT    NOT NULL,
    acked_ms  INTEGER NOT NULL,
    acked_by  TEXT    NOT NULL
);
