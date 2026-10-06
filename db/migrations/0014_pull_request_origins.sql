-- 0014_pull_request_origins: where each pull request came from, behind
-- GET/PUT /api/v1/repos/:id/pulls/:number/origin.
--
-- The GitHub repositories are read-only mirrors of this forge. An outside
-- contribution is opened there and brought here by an importer, which records
-- the pull request as `contributed`. A pull request with no row is `internal`,
-- which is what every pull request written inside the forge is. Only an
-- importer identity or a global admin writes a row, never the pull request's
-- own author, so the trust marking is not something a contribution can set
-- about itself.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/pull_origin.rs),
-- applied at startup by the shift migration runner (web/shift/heartbeats.rs),
-- which records each migration's sha256 in shift_schema_migrations and refuses
-- to start if an applied migration's file has changed. Migrations here are
-- immutable once released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE pull_request_origins` and delete
-- version 14 from shift_schema_migrations. Every pull request then reads as
-- `internal` and the contributed-merge gate stops holding anything, so drop it
-- only when no contribution is open.
-- Backfill: none; the table starts empty and the importer writes each row as
-- it brings a contribution in.
-- Lock safety: a new table in a telemetry-only file; no existing row is
-- touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table and
-- treats every pull request as internal, so an overlap must not run while a
-- contribution is open. The runner only checks the versions it knows, so the
-- schemas themselves can overlap.
-- No foreign keys: the pull request named here is a row in the forge core's
-- own store (a different file); the route resolves it on write, and a row for
-- a pull request that is gone is simply never read again. One row per pull
-- request, written by an importer or an admin, read by any caller who may read
-- the repository; access is decided per repository by the route, so no row
-- level security applies.

CREATE TABLE IF NOT EXISTS pull_request_origins (
    repo_owner  TEXT    NOT NULL CHECK (length(repo_owner) > 0),
    repo_name   TEXT    NOT NULL CHECK (length(repo_name) > 0),
    pr_number   INTEGER NOT NULL CHECK (pr_number > 0),
    origin      TEXT    NOT NULL CHECK (origin IN ('internal', 'contributed')),
    set_by      TEXT    NOT NULL CHECK (length(set_by) > 0),
    set_ms      INTEGER NOT NULL,
    PRIMARY KEY (repo_owner, repo_name, pr_number)
);
