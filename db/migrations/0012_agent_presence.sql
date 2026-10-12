-- 0012_agent_presence: which coding agents are working on which repositories
-- right now, behind POST/DELETE /api/v1/agent-presence and the reads
-- GET /api/v1/repos/:id/agent-presence and GET /api/v1/agent-presence
-- (docs/agent-presence.md, ADR 0006).
--
-- One row per reported session, upserted on every beat: a beat carries the
-- whole current record, so the row is replaced rather than appended to.
-- `repos` and `families` are JSON arrays of canonical `owner/name` and family
-- keys; a session scoped to a family is fanned out to that family's
-- repositories when it is read, never here, so it appears and disappears on
-- every repository at once. `tokens` and `links` are JSON objects or NULL.
-- `expires_ms` is `received_ms + ttl_seconds * 1000`; a row past it that never
-- ended reads as stale. `ended_ms` is when the session first reported (or was
-- ended as) `ended`, and is cleared if it comes back.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/agent_presence/store.rs),
-- applied at startup by the shift migration runner (web/shift/heartbeats.rs),
-- which records each migration's sha256 in shift_schema_migrations and refuses
-- to start if an applied migration's file has changed. Migrations here are
-- immutable once released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE agent_presence` and delete
-- version 12 from shift_schema_migrations. The table is a live view, not a
-- record: reporters re-register every session on their next beat (at most a
-- minute), so nothing is lost but the last few minutes of ended sessions.
-- Backfill: none; the table starts empty.
-- Lock safety: a new table and two indexes in a telemetry-only file; no
-- existing row is touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table, and
-- the runner only checks the versions it knows, so old and new can overlap.
-- Retention: rows not beaten for 14 days are deleted on write, like the
-- shift heartbeats.
-- No foreign keys: repository names and family keys are held in the forge
-- core's own store (a different file); the route validates them on write and
-- each read decides what a caller may see from the caller's repository
-- access, so no row level security applies here.

CREATE TABLE IF NOT EXISTS agent_presence (
    session_id      TEXT    PRIMARY KEY CHECK (length(session_id) BETWEEN 1 AND 64),
    reporter        TEXT    NOT NULL,
    agent           TEXT    NOT NULL,
    model           TEXT,
    host            TEXT,
    operator        TEXT,
    lane            TEXT,
    branch          TEXT,
    title           TEXT,
    state           TEXT    NOT NULL CHECK (state IN ('active', 'idle', 'ended')),
    repos_json      TEXT    NOT NULL,
    families_json   TEXT    NOT NULL,
    started_ms      INTEGER NOT NULL,
    last_active_ms  INTEGER NOT NULL,
    ttl_seconds     INTEGER NOT NULL CHECK (ttl_seconds BETWEEN 30 AND 3600),
    received_ms     INTEGER NOT NULL,
    expires_ms      INTEGER NOT NULL,
    ended_ms        INTEGER,
    cost_usd        REAL    CHECK (cost_usd IS NULL OR cost_usd >= 0),
    tokens_json     TEXT,
    links_json      TEXT
);

CREATE INDEX IF NOT EXISTS agent_presence_received ON agent_presence (received_ms);
CREATE INDEX IF NOT EXISTS agent_presence_expires ON agent_presence (expires_ms);
