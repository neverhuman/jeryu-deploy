-- 0013_repo_merge_settings: per-repository merge settings an administrator
-- sets through `PATCH /repos/{owner}/{repo}` (GitHub's spelling). The first
-- setting is `delete_branch_on_merge`: whether the forge deletes a pull
-- request's head branch after the pull request merges.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api
-- (github/merge_settings.rs), applied at startup by the shift migration runner
-- (web/shift/heartbeats.rs), which records each migration's sha256 in
-- shift_schema_migrations and refuses to start if an applied migration's file
-- has changed. Migrations here are immutable once released; numbering is a
-- zero-padded 4-digit sequence.
--
-- Rollback: stop the server, then `DROP TABLE repo_merge_settings` and delete
-- version 13 from shift_schema_migrations. Every repository falls back to the
-- default (delete the head branch on merge); no repository, branch or pull
-- request row is touched.
-- Backfill: none; the table starts empty and only admins write to it. A
-- repository with no row uses the default.
-- Lock safety: a new table in a telemetry-only file; no existing row is
-- touched and creation takes milliseconds.
-- Overlapping binaries: an older binary never reads or writes this table, and
-- the runner only checks the versions it knows, so old and new can overlap.
-- No foreign keys: `repo_id` names a repository held in the forge core's own
-- store (a different file). It is the repository's id rather than its name, so
-- a rename or transfer keeps the setting; a row for a deleted repository is
-- never read. One row per repository, written by an admin; no tenant-scoped
-- rows, so no row level security applies.

CREATE TABLE IF NOT EXISTS repo_merge_settings (
    repo_id                 TEXT    PRIMARY KEY CHECK (length(repo_id) > 0),
    delete_branch_on_merge  INTEGER NOT NULL CHECK (delete_branch_on_merge IN (0, 1)),
    updated_ms              INTEGER NOT NULL,
    updated_by              TEXT    NOT NULL
);
