-- 0010_cost_ledger: what the agent factory spent, kept for good.
--
-- pipeline_events is telemetry and is pruned after 30 days, but spend is a
-- record: GET /api/v1/yield divides it by the work that reached main, and a
-- month-old attempt or review still paid for that work. cost_ledger keeps the
-- cost-bearing columns of every event that carried a cost_usd and is never
-- pruned. EventStore::insert writes the ledger row in the same transaction as
-- the event; a repeated event_id writes neither.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/pipeline/store.rs),
-- applied at startup by the shift migration runner (web/shift/heartbeats.rs),
-- which records each migration's sha256 in shift_schema_migrations and refuses
-- to start if an applied migration's file has changed. Migrations here are
-- immutable once released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE cost_ledger` and delete version
-- 10 from shift_schema_migrations. Costs recorded after the upgrade and older
-- than 30 days are then gone; take a copy of shift.sqlite first (sqlite3
-- shift.sqlite '.backup cost-ledger.sqlite') to keep them.
-- Backfill: one INSERT ... SELECT copies every pipeline_events row with a
-- cost_usd, oldest first. It is bounded by the 30-day retention of the source
-- table, so it reads at most a month of events, in the same transaction as the
-- CREATE TABLE.
-- Lock safety: a new table and its indexes; the backfill only reads
-- pipeline_events. The write lock is held for the one migration transaction,
-- which takes milliseconds for a month of events.
-- Overlapping binaries: an older binary never reads or writes this table, and
-- the runner only checks the versions it knows, so old and new can overlap;
-- events an older binary stores during the overlap are not copied.
-- No foreign keys (join keys are plain text, as in pipeline_events) and no
-- tenant-scoped rows: reads are admin-only, so no row level security applies.
-- The partial UNIQUE index on (reporter, event_id) mirrors the event log's:
-- a producer that names its event is recorded once, and a retry of an event
-- pruned from the log is not counted twice.

CREATE TABLE IF NOT EXISTS cost_ledger (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    reporter    TEXT    NOT NULL,
    event_id    TEXT,
    ts_ms       INTEGER NOT NULL,
    source      TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    family      TEXT,
    repo        TEXT,
    pr          INTEGER,
    sha         TEXT,
    todo_id     TEXT,
    shift       TEXT,
    outcome     TEXT,
    cost_usd    REAL    NOT NULL CHECK (cost_usd >= 0),
    seconds     INTEGER,
    detail_json TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS cost_ledger_event_id
    ON cost_ledger (reporter, event_id) WHERE event_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS cost_ledger_kind_repo_pr
    ON cost_ledger (kind, repo, pr);

CREATE INDEX IF NOT EXISTS cost_ledger_todo
    ON cost_ledger (todo_id);

INSERT OR IGNORE INTO cost_ledger (reporter, event_id, ts_ms, source, kind, family, repo, pr,
    sha, todo_id, shift, outcome, cost_usd, seconds, detail_json)
SELECT reporter, event_id, ts_ms, source, kind, family, repo, pr,
    sha, todo_id, shift, outcome, cost_usd, seconds, detail_json
  FROM pipeline_events
 WHERE cost_usd IS NOT NULL
 ORDER BY seq;
