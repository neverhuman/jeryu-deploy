-- The initial reconciler parsed a full split lock as a single TOML value.
-- Direct deploy membership was correct, but cached success omitted pinned web
-- membership. Replay retained Jeryu deploy events through the corrected reader;
-- per-lot receipt keys keep already recorded transitions idempotent.
DELETE FROM flow_delivery_reads
WHERE event_seq IN (
    SELECT seq FROM pipeline_events WHERE repo LIKE '%/jeryu-deploy'
);
