-- 0016_pipeline_event_families: backfill `family` on the pipeline event rows
-- that landed without one.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/pipeline/store.rs),
-- applied at startup by the shift migration runner (web/shift/heartbeats.rs).
-- Immutable once released.
--
-- Why: only the queue and shift emitters set `family`, so every gate.*,
-- review.*, deploy.*, release.* and pin.* row stored `family` NULL even though
-- it named a repo. The Activity page's family filter is a column filter, so
-- `?family=<x>&kind=gate.` answered nothing and the page said "Nothing for <x>
-- here" while the same repo's gates were right there. New rows are fixed at
-- write time (`derive_family` in web/pipeline.rs resolves repo -> family from
-- the repository registry); this fills the rows already stored.
--
-- Derivation: a migration is plain SQL and cannot read the registry, so it
-- uses the log's own evidence -- the family some other row already recorded
-- for the same repo. A repo whose rows all lack a family keeps NULL; those
-- rows age out within the log's 30-day retention and their successors are
-- derived at write time. The `MIN(family)` picks one key for a repo that two
-- rows disagree about, which keeps the update deterministic; repos do not
-- change family inside one retention window.
--
-- Backfill: bounded by the rows in a 30-day telemetry log (~12k rows when this
-- was written), one UPDATE in the migration transaction, milliseconds. The
-- pipeline_events_repo_pr and pipeline_events_family indexes serve both halves.
-- Rollback: `UPDATE pipeline_events SET family = NULL WHERE ...` is not
-- recoverable row by row, so roll back by deleting version 13 from
-- shift_schema_migrations and running the previous binary: a wrongly derived
-- family is cosmetic (a filter chip) in a telemetry-only table, and nothing of
-- record lives here. Fix-forward is to re-run this statement.
-- Lock safety: writes only pipeline_events, a table no older binary reads.
-- Overlapping binaries: an old binary writes NULL families again and a new one
-- derives them; both read the column the same way, so the two can overlap.

UPDATE pipeline_events
   SET family = (SELECT MIN(known.family)
                   FROM pipeline_events known
                  WHERE known.repo = pipeline_events.repo
                    AND known.family IS NOT NULL)
 WHERE family IS NULL
   AND repo IS NOT NULL;
