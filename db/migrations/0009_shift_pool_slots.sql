-- 0009_shift_pool_slots: what an external worker pool reports beside a todoq
-- slot. A pool slot is one account, container or browser profile of a fleet
-- the forge does not run; it holds several jobs at once and it can be
-- unavailable for a reason no todoq slot has (a sign-in it needs back).
--
-- Four columns on shift_heartbeats: `kind` tells a pool slot from a todoq one
-- ('todoq' for every row written before this), `detail` is the short reason
-- behind the reported state, and `capacity`/`active` are how many jobs the
-- slot can hold and holds now. Without them /work can only say that a slot is
-- paused, not why, nor how loaded the pool is.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/shift/heartbeats.rs),
-- applied at startup by that module's migration runner, which records each
-- migration's sha256 in shift_schema_migrations and refuses to start if an
-- applied migration's file has changed. Migrations here are immutable once
-- released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server and delete shift.sqlite, or keep the columns and
-- delete version 9 from shift_schema_migrations after dropping them (SQLite
-- 3.35+ supports DROP COLUMN). The table holds telemetry only (14-day
-- retention, pruned by the server), so nothing of record is lost.
-- Backfill: none beyond the DEFAULT, which gives every stored row
-- kind='todoq' -- what every reporter before this change meant.
-- Lock safety: four ALTER TABLE ADD COLUMN on a telemetry-only table; SQLite
-- rewrites no rows for an added column with a constant default, so the whole
-- migration takes milliseconds.
-- Overlapping binaries: an older binary writes no `kind`, and the DEFAULT
-- makes its rows todoq slots, which is what they are. A newer binary reading
-- them sees the same. No foreign keys and no tenant-scoped rows, so no row
-- level security applies.

ALTER TABLE shift_heartbeats ADD COLUMN kind TEXT NOT NULL DEFAULT 'todoq';
ALTER TABLE shift_heartbeats ADD COLUMN detail TEXT;
ALTER TABLE shift_heartbeats ADD COLUMN capacity INTEGER;
ALTER TABLE shift_heartbeats ADD COLUMN active INTEGER;
