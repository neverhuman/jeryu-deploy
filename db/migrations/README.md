# Migrations

Numbered, immutable SQL migrations for the SQLite stores jeryu-api owns.
Numbering is a zero-padded four-digit sequence (`0001_<name>.sql`). The store
that owns a migration embeds it and records its sha256 when applied; a changed
file for an applied version stops the server at startup.

| Migration | Store | Owner |
|---|---|---|
| `0001_shift_heartbeats.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/shift/heartbeats.rs` |
| `0002_pipeline_events.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/pipeline/store.rs` |
| `0003_jankurai_disputes.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/jankurai/disputes.rs` |
| `0004_work_bridge_repairs.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/github/work_bridge_repairs.rs` |

Each migration carries its rollback, backfill, and lock-safety notes in its
header comment. Every later migration must be ordered, immutable after
release, and checksum-bound in the release receipt. The same reviewed change
must provide:

- a restorable pre-migration backup and restore rehearsal;
- the forward command plus a tested rollback or explicit fix-forward command;
- bounded backfill batches and maximum lock duration;
- compatibility expectations for overlapping binary versions; and
- deterministic post-migration schema, constraint, row-count, and read checks.

A release that ships a new migration declares it (and its checksum) instead of
`data_migration=none`. A missing checksum, backup, recovery command, or
verification result is a deployment stop condition.
