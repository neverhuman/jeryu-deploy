-- 0027_line_survival: how many of the lines a change added are still on main
-- 14 and 30 days after it merged.
--
-- The Scorecard's "Surviving net lines/day" counts lines a change added that
-- `git blame` on the repository's default branch still attributes to it,
-- because raw net lines reward churn and stubs. A background job
-- (web/line_survival.rs) measures one merged repository leg at a time, never
-- on a request, and writes the result on the leg's outcome_ledger_repos row:
--   lines_added, lines_deleted  what the merge diff changed, generated,
--                               vendored and lock files left out
--   surviving_14d               added lines still blamed on the change at the
--   surviving_30d               last main commit 14 / 30 days after the merge
-- Every column is NULL until measured. NULL means "not measured yet" and is
-- never read as zero: the Scorecard shows such a change as pending.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/line_survival.rs
-- measures, web/pipeline/store.rs writes), applied at startup by the shift
-- migration runner (web/shift/heartbeats.rs).
--
-- Rollback: roll back with the binary; the columns are ignored by an older
-- binary, so leaving them is harmless. To drop them, restore the pre-release
-- shift.sqlite backup. Delete version 27 from shift_schema_migrations only
-- together with that restore.
-- Backfill: none in SQL. The job measures every merged leg whose 14 or 30 day
-- mark has passed, oldest first, so downtime and a first start catch up on
-- their own. Clearing the four columns makes the job measure the leg again.
-- Lock safety: four nullable ALTER TABLE ADD COLUMN statements; no row is
-- rewritten.
-- Overlapping binaries: an older binary neither reads nor writes the columns
-- (its upserts name their own columns), so old and new can overlap.

ALTER TABLE outcome_ledger_repos ADD COLUMN lines_added INTEGER;
ALTER TABLE outcome_ledger_repos ADD COLUMN lines_deleted INTEGER;
ALTER TABLE outcome_ledger_repos ADD COLUMN surviving_14d INTEGER;
ALTER TABLE outcome_ledger_repos ADD COLUMN surviving_30d INTEGER;
