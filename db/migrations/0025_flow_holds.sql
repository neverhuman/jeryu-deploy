-- A lot that stopped stays at its station, held, with why and who holds it.
-- Four nullable columns on flow_ledger; existing rows keep NULL (no reason
-- recorded) and read as a plain hold. busy_timeout=5s; no row is rewritten.
-- Rollback: restore the pre-release shift.sqlite backup and previous binary.
ALTER TABLE flow_ledger ADD COLUMN hold_reason TEXT;
ALTER TABLE flow_ledger ADD COLUMN hold_by TEXT;
ALTER TABLE flow_ledger ADD COLUMN hold_since_ms INTEGER;
ALTER TABLE flow_ledger ADD COLUMN hold_sha TEXT;
