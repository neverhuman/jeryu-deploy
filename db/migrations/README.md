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
| `0005_site_settings.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/site_settings.rs` |
| `0006_attention_acks.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/pipeline/attention/acks.rs` |
| `0007_idempotency_keys.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/idempotency.rs` |
| `0008_agent_run_intents.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/agent_runs/intents.rs` |
| `0009_shift_pool_slots.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/shift/heartbeats.rs` |
| `0010_cost_ledger.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/pipeline/store.rs` |
| `0011_flow_ledger.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/pipeline/store.rs` |
| `0012_agent_presence.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/agent_presence/store.rs` |
| `0013_repo_merge_settings.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/github/merge_settings.rs` |
| `0014_pull_request_origins.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/pull_origin.rs` |
| `0015_usage_samples.sql` | `<data_dir>/shift.sqlite` | `crates/jeryu-api/src/web/usage.rs` |

Each migration carries its rollback, backfill, and lock-safety notes in its
header comment. A migration that alters an existing table also has a
`<stem>.meta.toml` beside it declaring its lock and statement bounds. SQLite
has no `lock_timeout` or `statement_timeout` to set in the SQL: the lock wait
is the store's `busy_timeout`, and the statement bound comes from the change's
shape. The SQL file stays checksum-bound; the metadata is not embedded. Every later migration must be ordered, immutable after
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
