-- 0011_flow_ledger: where every lot sat at every station, kept for good.
--
-- GET /api/v1/flow answers how much work waits at each step of the delivery
-- line, how long it waits against how long the step works, where it loops
-- back and which step is the bottleneck (docs/flow.md, ADR 0004). None of
-- that can be read from pipeline_events at request time: the log is pruned
-- after 30 days, so a 90-day window would quietly shrink, and the station a
-- lot sits at is a fold over its whole event sequence, not one row.
--
-- flow_ledger is that fold, projected as events are stored (the same
-- transaction as the event, like cost_ledger) and never pruned. One row is
-- one visit of one lot to one station: a lot that went round the gate twice
-- has two Gate rows, so the loop is countable. A lot is a todo, or
-- <owner>/<repo>#<pr> for a pull request no todo carries; the projection
-- rewrites the second onto the first when an event names both.
--
-- `shift` is not in the design's column list but the projection needs it:
-- `pr.opened` carries no todo id, so the only join from a shift pull request
-- back to the lots it carries is the shift branch the work landed on. Shift
-- branch names are shared by every repository and family, so the join is
-- the branch *and* the repository (and the family, when the event names
-- one): a lot that recorded no repository is never swept by a shift PR.
--
-- flow_ledger_events is the projection's idempotency key, never pruned like
-- the ledger: one row per (reporter, event_id) the projection has folded in.
-- The event log forgets an event_id when it prunes the row after 30 days, so
-- a producer re-posting the same event later would otherwise be projected a
-- second time and count a second visit; cost_ledger's unique key answers the
-- same problem for spend.
--
-- `direction` is `open` while the visit is in flight, `hold` when a person
-- must act before it can move, and `forward` or `rework` once it left;
-- `rework_to` names the station a rework visit went back to.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/pipeline/store.rs
-- writes it, web/flow/project.rs decides what to write), applied at startup
-- by the shift migration runner (web/shift/heartbeats.rs), which records each
-- migration's sha256 in shift_schema_migrations and refuses to start if an
-- applied migration's file has changed. Migrations here are immutable once
-- released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE flow_ledger`, `DROP TABLE
-- flow_ledger_events`, `DROP TABLE flow_ledger_replay` and delete version 11
-- from shift_schema_migrations.
-- Nothing of record is lost: the ledger is derived, and the next start
-- replays it from the events the log still holds. Take a copy first (sqlite3
-- shift.sqlite '.backup flow-ledger.sqlite') to keep visits older than the
-- log's 30-day retention.
-- Backfill: not in SQL. The projection is a Rust fold over the event
-- sequence, so the replay runs in EventStore::open as soon as this migration
-- has applied: it reads pipeline_events oldest first, projects every event
-- into the empty table and records one flow_ledger_replay row, which is what
-- makes it happen exactly once. A projection bug is fixed by truncating all
-- three tables and restarting, never by editing rows.
-- Lock safety: three new tables and their indexes; creation takes milliseconds
-- and touches no existing row. The replay that follows is bounded by the
-- 30-day retention of pipeline_events, so it reads at most a month of events,
-- once, in one transaction.
-- Overlapping binaries: an older binary never reads or writes these tables,
-- and the runner only checks the versions it knows, so old and new can
-- overlap; events an older binary stores during the overlap are not
-- projected, and are recovered by a truncate-and-replay.
-- No foreign keys (join keys are plain text, as in pipeline_events: a visit
-- outlives the todo or pull request it names) and no tenant-scoped rows:
-- reads are admin-only, so no row level security applies.

CREATE TABLE IF NOT EXISTS flow_ledger (
    seq              INTEGER PRIMARY KEY AUTOINCREMENT,
    lot_id           TEXT    NOT NULL,
    family           TEXT,
    repo             TEXT,
    pr               INTEGER,
    todo_id          TEXT,
    shift            TEXT,
    station          TEXT    NOT NULL,
    entered_ms       INTEGER NOT NULL,
    started_ms       INTEGER,
    left_ms          INTEGER,
    outcome          TEXT,
    direction        TEXT    NOT NULL
                     CHECK (direction IN ('open', 'hold', 'forward', 'rework')),
    rework_to        TEXT,
    source_seq_first INTEGER NOT NULL,
    source_seq_last  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS flow_ledger_lot
    ON flow_ledger (lot_id, station, seq);

CREATE INDEX IF NOT EXISTS flow_ledger_station
    ON flow_ledger (station, entered_ms);

CREATE INDEX IF NOT EXISTS flow_ledger_open
    ON flow_ledger (station, lot_id) WHERE left_ms IS NULL;

CREATE INDEX IF NOT EXISTS flow_ledger_shift
    ON flow_ledger (shift) WHERE shift IS NOT NULL;

CREATE INDEX IF NOT EXISTS flow_ledger_repo_pr
    ON flow_ledger (repo, pr);

-- GET /api/v1/flow reads only the lots still on the line or that left a
-- station inside its window (at most 90 days): this index finds them
-- without scanning the never-pruned ledger.
CREATE INDEX IF NOT EXISTS flow_ledger_left
    ON flow_ledger (left_ms);

-- Whether a family's lots ever reached a station (a station only visited
-- before the window is "reached", not "no_events"): one probe per station.
CREATE INDEX IF NOT EXISTS flow_ledger_station_family
    ON flow_ledger (station, family);

-- One row per event the projection has folded in, so an event re-posted
-- after the event log pruned it is not projected twice.
CREATE TABLE IF NOT EXISTS flow_ledger_events (
    reporter TEXT    NOT NULL,
    event_id TEXT    NOT NULL,
    seq      INTEGER NOT NULL,
    PRIMARY KEY (reporter, event_id)
);

-- One row once the replay has run, so a restart never projects the retained
-- log a second time on top of the visits it already built.
CREATE TABLE IF NOT EXISTS flow_ledger_replay (
    replayed_ms INTEGER NOT NULL,
    events      INTEGER NOT NULL,
    visits      INTEGER NOT NULL
);
