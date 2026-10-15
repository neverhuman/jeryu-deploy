-- 0023_change_outcomes: what each change lot ended as, and the human minutes
-- charged to it, kept for good.
--
-- The Scorecard reports engineering time per successful outcome
-- (docs/pipeline-events.md, "Human minutes per change"). Neither half can be
-- read from pipeline_events at request time: the log is pruned after 30 days,
-- and a person's minutes are a fold over the gaps between their own actions,
-- not one row. Both tables are written in the same transaction as the thing
-- they record, and neither is ever pruned.
--
-- change_outcomes is one row per change lot (a todo id, or <owner>/<repo>#<N>
-- for a pull request no todo carries, exactly as flow_ledger spells a lot).
-- `landed_ms` is when the change reached main; `failed_ms` is when the first
-- defect was linked to it. A change is *successful* when it landed and no
-- defect was linked to it; a change named by `incident.caused_by` is failed
-- through incident_causes (migration 0024) as well.
-- `work_kind` is what kind of work the lot was: `change` by default, `rework`
-- when a todo was filed to redo another change, `pin_bump` for an auto-pin
-- dependency bump.
--
-- change_human_minutes is one row per (lot, source, actor, UTC day). `source`
-- labels where the minutes came from, so a reader can say what is and is not
-- counted: `review` (a person reading a pull request's files and submitting a
-- review or a comment), `commit` (a commit a person authored on the lot's
-- branch), `session` (an interactive agent session a person drove) and
-- `triage` (a todo action a person took). `actor` is kept only so two people
-- working the same lot at the same time are two sets of gaps, never to report
-- on a person: every read aggregates by family, repository and work kind.
-- `last_ms` is the actor's newest mark in that day, which is what the next
-- mark measures its gap against; `marks` is how many marks the row folded.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/human_time.rs),
-- applied at startup by the shift migration runner (web/shift/heartbeats.rs),
-- which records each migration's sha256 in shift_schema_migrations and
-- refuses to start if an applied migration's file has changed. Migrations
-- here are immutable once released; numbering is a zero-padded 4-digit
-- sequence.
--
-- Rollback: stop the server, then `DROP TABLE change_human_minutes`,
-- `DROP TABLE change_outcomes` and delete version 23 from
-- shift_schema_migrations; roll back together with the binary. Take a copy
-- first (sqlite3 shift.sqlite '.backup change-outcomes.sqlite'): minutes are
-- a record of time people spent and cannot be rebuilt from the pruned log.
-- Backfill: none. The tables start empty and fill as work happens; a window
-- that reaches before this migration reports the minutes it has.
-- Lock safety: two new tables and their indexes over no rows; creation takes
-- milliseconds and touches no existing row.
-- Overlapping binaries: an older binary never reads or writes these tables,
-- and the runner only checks the versions it knows, so old and new can
-- overlap; minutes an older binary serves during the overlap are not
-- recorded, and are simply missing rather than wrong.
-- No foreign keys (lot keys are plain text, as in flow_ledger: an outcome
-- outlives the todo or pull request it names) and no tenant-scoped rows:
-- reads are admin-only and aggregated, so no row level security applies.

CREATE TABLE IF NOT EXISTS change_outcomes (
    lot_id    TEXT    PRIMARY KEY,
    family    TEXT,
    repo      TEXT,
    work_kind TEXT    NOT NULL DEFAULT 'change'
              CHECK (work_kind IN ('change', 'rework', 'pin_bump')),
    landed_ms INTEGER,
    failed_ms INTEGER,
    first_ms  INTEGER NOT NULL,
    last_ms   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS change_outcomes_landed
    ON change_outcomes (landed_ms);

CREATE INDEX IF NOT EXISTS change_outcomes_family
    ON change_outcomes (family, landed_ms);

CREATE TABLE IF NOT EXISTS change_human_minutes (
    lot_id  TEXT    NOT NULL,
    source  TEXT    NOT NULL
            CHECK (source IN ('review', 'commit', 'session', 'triage')),
    actor   TEXT    NOT NULL,
    day     TEXT    NOT NULL,
    minutes REAL    NOT NULL CHECK (minutes >= 0),
    marks   INTEGER NOT NULL,
    last_ms INTEGER NOT NULL,
    PRIMARY KEY (lot_id, source, actor, day)
);

CREATE INDEX IF NOT EXISTS change_human_minutes_day
    ON change_human_minutes (day);
