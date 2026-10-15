-- Private optional session metadata. Existing rows remain NULL; no backfill.
-- Two constant-time nullable column additions under the store's five-second
-- busy timeout. No new foreign keys; JSON is validated at the authenticated API.
-- SQLite has no row-level security: reads strip location/worker metadata for
-- everyone except the session reporter and global admins.
-- Earlier binaries ignore the new columns and remain compatible. Roll back the
-- binary without removing columns, or restore the pre-release SQLite backup.
ALTER TABLE agent_presence ADD COLUMN location_json TEXT;
ALTER TABLE agent_presence ADD COLUMN worker_json TEXT;
