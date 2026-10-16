-- 0026_pull_request_replacements: what replaced a pull request, behind
-- PATCH /api/v3/repos/<o>/<r>/pulls/<n> with {state: closed, superseded_by: m}
-- and POST /api/v1/repos/:id/pulls/:n/close with {"superseded_by": m}.
--
-- An agent that cannot push to a frozen or contested branch lands the work as
-- a second pull request cherry-picked onto the base and closes the first. One
-- row records that: the replaced pull request and the one that replaced it,
-- both in the same repository. One row per replaced pull request, so a
-- replacement may carry several; the route refuses a link to another
-- repository, to the pull request itself, or one that closes a cycle.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api
-- (github/pull_replacements.rs), applied at startup by the shift migration
-- runner (web/shift/heartbeats.rs), which records each migration's sha256 and
-- refuses to start if an applied migration's file has changed. Migrations here
-- are immutable once released; numbering is a zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE pull_request_replacements` and
-- delete version 26 from shift_schema_migrations. Every pull request then reads
-- with no replacement link, as it did before this migration, and the pointer
-- comments the closes left behind remain the record.
-- Backfill: the table starts empty; at startup the server links the pull
-- requests whose own closing comment, title or body already names what
-- replaced them, and records nothing for the rest.
-- Lock safety: a new table in a telemetry-only file; no existing row is
-- touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table and
-- shows a replaced pull request as it did before, so an overlap is safe. The
-- runner only checks the versions it knows, so the schemas themselves can
-- overlap.
-- No foreign keys: the pull requests named here are rows in the forge core's
-- own store (a different file); the route resolves both on write, and a row for
-- a pull request that is gone is simply never read again. Read by any caller
-- who may read the repository; access is decided per repository by the route,
-- so no row level security applies.

CREATE TABLE IF NOT EXISTS pull_request_replacements (
    repo_owner         TEXT    NOT NULL CHECK (length(repo_owner) > 0),
    repo_name          TEXT    NOT NULL CHECK (length(repo_name) > 0),
    pr_number          INTEGER NOT NULL CHECK (pr_number > 0),
    replacement_number INTEGER NOT NULL CHECK (replacement_number > 0),
    set_by             TEXT    NOT NULL CHECK (length(set_by) > 0),
    set_ms             INTEGER NOT NULL,
    PRIMARY KEY (repo_owner, repo_name, pr_number),
    CHECK (replacement_number <> pr_number)
);
