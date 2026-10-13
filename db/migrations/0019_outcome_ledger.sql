-- 0019_outcome_ledger: what became of every change, kept for good.
--
-- The Scorecard (docs/scorecard.md, ADR 0010) judges delivery by outcomes:
-- change failure rate, false-done rate, rework rate, changes per release and
-- the cost per successful change all need one durable row per change that
-- says when it was filed, merged and shipped, and whether it was later
-- reverted, rolled back, reworked or blamed for an incident. None of that can
-- be read from pipeline_events at request time: the log is pruned after 30
-- days and the 14-day observation horizon of a 90-day window reaches past it.
--
-- outcome_ledger is that projection, written in the same transaction as each
-- event (like cost_ledger and flow_ledger) and never pruned. One row is one
-- change: a todo, or <owner>/<repo>#<pr> for a pull request no todo carries,
-- the same lot key flow_ledger uses; the projection rewrites the second onto
-- the first when an event names both. A change that spans repositories is one
-- row with one outcome_ledger_repos row per repository.
--
-- The derived outcome (pending, successful, failed, reworked) is not stored:
-- "pending" depends on the clock and a rework link may arrive after the
-- 14-day horizon, so the report derives it from these facts at read time.
-- reworks, reworked_by and incidents are JSON arrays of text, kept as sorted
-- sets so a re-posted event changes nothing.
--
-- outcome_ledger_shas maps a merged commit to its change, so a later revert
-- naming only a sha finds the change it undoes. outcome_ledger_events is the
-- idempotency key (one row per (reporter, event_id) folded in), never pruned,
-- for the same reason as flow_ledger_events.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/pipeline/store.rs
-- writes it, web/outcomes/project.rs decides what to write), applied at
-- startup by the shift migration runner (web/shift/heartbeats.rs).
--
-- Rollback: stop the server, then DROP TABLE outcome_ledger, outcome_ledger_repos,
-- outcome_ledger_shas, outcome_ledger_events and outcome_ledger_replay and
-- delete version 19 from shift_schema_migrations. The ledger is derived; the
-- next start replays the events the log still holds. Take a copy first
-- (sqlite3 shift.sqlite '.backup outcome-ledger.sqlite') to keep outcomes
-- older than the log's 30-day retention.
-- Backfill: not in SQL. The replay runs in EventStore::open once this
-- migration has applied (outcome_ledger_replay makes it happen once); a
-- projection bug is fixed by truncating all five tables and restarting.
-- Lock safety: five new tables; creation touches no existing row.
-- Overlapping binaries: an older binary never reads or writes these tables;
-- events it stores during the overlap are recovered by truncate-and-replay.
-- No foreign keys and no tenant-scoped rows: reads are admin-only.

CREATE TABLE IF NOT EXISTS outcome_ledger (
    lot_id         TEXT    PRIMARY KEY,
    family         TEXT,
    todo_id        TEXT,
    work_kind      TEXT,
    first_seen_ms  INTEGER NOT NULL,
    filed_ms       INTEGER,
    done_ms        INTEGER,
    agent          TEXT,
    model          TEXT,
    experiment     TEXT,
    reverted_ms    INTEGER,
    reverted_by    TEXT,
    rolled_back_ms INTEGER,
    reworks        TEXT    NOT NULL DEFAULT '[]',
    reworked_by    TEXT    NOT NULL DEFAULT '[]',
    incidents      TEXT    NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS outcome_ledger_family
    ON outcome_ledger (family);

CREATE TABLE IF NOT EXISTS outcome_ledger_repos (
    lot_id      TEXT    NOT NULL,
    repo        TEXT    NOT NULL,
    pr          INTEGER,
    sha         TEXT,
    merged_ms   INTEGER,
    released_ms INTEGER,
    deployed_ms INTEGER,
    release     TEXT,
    PRIMARY KEY (lot_id, repo)
);

CREATE INDEX IF NOT EXISTS outcome_ledger_repos_repo
    ON outcome_ledger_repos (repo, merged_ms);

CREATE TABLE IF NOT EXISTS outcome_ledger_shas (
    sha    TEXT PRIMARY KEY,
    repo   TEXT,
    lot_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS outcome_ledger_events (
    reporter TEXT    NOT NULL,
    event_id TEXT    NOT NULL,
    seq      INTEGER NOT NULL,
    PRIMARY KEY (reporter, event_id)
);

CREATE TABLE IF NOT EXISTS outcome_ledger_replay (
    replayed_ms INTEGER NOT NULL,
    events      INTEGER NOT NULL,
    changes     INTEGER NOT NULL
);
