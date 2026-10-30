-- Build identity and reported artifact/deployment observations; no backfill.
-- Additive and compatible with earlier binaries, which ignore these tables.
-- Lock: CREATE TABLE/INDEX only, bounded by the store busy_timeout.
-- Before activation use the normal shift.sqlite backup/restore rehearsal.
-- Rollback: run the prior binary, retaining these tables for forward recovery.
-- Verify: migration checksum plus durable restart/transaction tests in build_inventory.
CREATE TABLE IF NOT EXISTS build_inventory (
    family TEXT NOT NULL,
    id TEXT NOT NULL,
    deliverable TEXT NOT NULL,
    built_at TEXT NOT NULL,
    identity TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (family, id)
);
CREATE INDEX IF NOT EXISTS build_inventory_order ON build_inventory(family, deliverable, built_at DESC, id);
CREATE TABLE IF NOT EXISTS build_placements (
    family TEXT PRIMARY KEY,
    observed_at TEXT NOT NULL,
    body TEXT NOT NULL
);
