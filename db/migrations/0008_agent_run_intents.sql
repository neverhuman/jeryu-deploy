-- 0008_agent_run_intents: the intent of every agent run the forge has
-- acknowledged. One row per run id, written before the start answer leaves the
-- handler, holding what the caller asked to run (kind, owning repository,
-- program, args, workspace) and when it was recorded. `agent_run_id_sequence`
-- hands out the run ids themselves.
--
-- The run id came from a process counter and the record lived only in memory,
-- so `POST /api/v1/agent-runs` answered 201 with an id that a restart erased
-- and that a later process handed out again to a different run. Reading the id
-- from a durable sequence, in the same transaction that records the intent,
-- makes an acknowledged id a thing that exists on disk and is never reused.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api
-- (web/agent_runs/intents.rs), applied at startup by the shift migration
-- runner (web/shift/heartbeats.rs), which records each migration's sha256 in
-- shift_schema_migrations and refuses to start if an applied migration's file
-- has changed. Migrations here are immutable once released; numbering is a
-- zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE agent_run_intents`, `DROP TABLE
-- agent_run_id_sequence` and delete version 8 from shift_schema_migrations.
-- Run ids are then counted in memory again and a start is acknowledged with
-- nothing on disk behind it; no other table is touched.
-- Backfill: none. The table starts empty; runs that were only ever in memory
-- have no intent to recover, and the sequence starts at 1.
-- Lock safety: two new tables in a telemetry-only file plus one seed row; no
-- existing row is touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes these tables and
-- keeps counting ids in memory, so while it runs its ids may collide with ids
-- the sequence has already handed out; the runner only checks the versions it
-- knows, so old and new can overlap.
-- No foreign keys: `repo` names a repository by `owner/name`, which is how the
-- rest of the shift store spells one, and an intent of a repository that has
-- since been removed is still the truth about what was acknowledged. Rows are
-- dropped once they are 14 days old, the retention the rest of this file uses.
-- One row per acknowledged run, readable only by forge operators, so no row
-- level security applies.

CREATE TABLE IF NOT EXISTS agent_run_intents (
    run_id      TEXT    PRIMARY KEY CHECK (length(run_id) > 0),
    -- Which surface acknowledged it: agent_run, session or shell.
    kind        TEXT    NOT NULL CHECK (length(kind) > 0),
    -- Owning repository `owner/name`, NULL for a run that owns no repository.
    repo        TEXT,
    program     TEXT    NOT NULL,
    -- The argument vector, a JSON array of strings.
    args_json   TEXT    NOT NULL,
    -- Where the run works, NULL for a session whose workspace is named after
    -- the run id and so is not known until the id has been recorded.
    workspace   TEXT,
    recorded_ms INTEGER NOT NULL
);

-- The durable run-id counter: one row, holding the next id to hand out. Kept
-- apart from the intents so pruning old intents can never replay an id.
CREATE TABLE IF NOT EXISTS agent_run_id_sequence (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    next_seq INTEGER NOT NULL CHECK (next_seq > 0)
);

INSERT OR IGNORE INTO agent_run_id_sequence (id, next_seq) VALUES (1, 1);
