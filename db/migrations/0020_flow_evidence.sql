-- Durable station transitions and immutable delivery membership.
-- Additive SQLite tables, one short migration transaction; busy_timeout=5s.
-- No foreign keys: evidence outlives the 30-day pipeline log. Admin-only reads.
-- Backfill copies measured arrival/start/exit times; historical hold transitions
-- cannot be reconstructed and are explicitly excluded from historical coverage.
-- Rollback: restore the pre-release shift.sqlite backup and previous binary.
CREATE TABLE flow_activity (
    event_seq INTEGER NOT NULL, lot_id TEXT NOT NULL, station TEXT NOT NULL,
    state TEXT NOT NULL, ts_ms INTEGER NOT NULL, family TEXT, repo TEXT, pr INTEGER,
    sha TEXT, summary TEXT, runner TEXT, log_url TEXT,
    PRIMARY KEY(event_seq, lot_id, station, state)
);
CREATE INDEX flow_activity_window ON flow_activity(station, ts_ms, family);
CREATE INDEX flow_activity_lot ON flow_activity(lot_id, ts_ms);
-- An authoritative merge resolves earlier unfinished visits of that same PR.
-- Preserve missing start times; the merge observation is the exit evidence.
UPDATE flow_ledger AS v SET
 left_ms=(SELECT MIN(s.entered_ms) FROM flow_ledger s JOIN pipeline_events e ON e.seq=s.source_seq_first WHERE s.repo=v.repo AND s.pr=v.pr AND s.station='stage' AND s.entered_ms>=v.entered_ms AND e.kind IN ('pr.merged','queue.landed','todo.merged')),
 source_seq_last=(SELECT s.source_seq_first FROM flow_ledger s JOIN pipeline_events e ON e.seq=s.source_seq_first WHERE s.repo=v.repo AND s.pr=v.pr AND s.station='stage' AND s.entered_ms>=v.entered_ms AND e.kind IN ('pr.merged','queue.landed','todo.merged') ORDER BY s.entered_ms LIMIT 1),
 direction='forward'
WHERE v.left_ms IS NULL AND v.station IN ('queue','work','shift','review','gate','merge')
 AND EXISTS(SELECT 1 FROM flow_ledger s JOIN pipeline_events e ON e.seq=s.source_seq_first WHERE s.repo=v.repo AND s.pr=v.pr AND s.station='stage' AND s.entered_ms>=v.entered_ms AND e.kind IN ('pr.merged','queue.landed','todo.merged'));
INSERT OR IGNORE INTO flow_activity
SELECT source_seq_first, lot_id, station, 'arrived', entered_ms, family, repo, pr,
       NULL, NULL, NULL, NULL FROM flow_ledger;
INSERT OR IGNORE INTO flow_activity
SELECT source_seq_last, lot_id, station, 'started', started_ms, family, repo, pr,
       NULL, NULL, NULL, NULL FROM flow_ledger
WHERE started_ms IS NOT NULL AND station NOT IN ('shift', 'stage');
INSERT OR IGNORE INTO flow_activity
SELECT source_seq_last, lot_id, station, CASE direction WHEN 'forward' THEN 'passed' ELSE 'rework' END,
       left_ms, family, repo, pr, NULL, NULL, NULL, NULL FROM flow_ledger
WHERE left_ms IS NOT NULL;
CREATE TABLE flow_evidence_meta (version INTEGER PRIMARY KEY, since_ms INTEGER NOT NULL);
INSERT INTO flow_evidence_meta VALUES (1, CAST(strftime('%s','now') AS INTEGER)*1000);
CREATE TABLE flow_merge_revisions (
    repo TEXT NOT NULL, pr INTEGER NOT NULL, sha TEXT NOT NULL, family TEXT,
    PRIMARY KEY(repo, pr, sha)
);
INSERT OR IGNORE INTO flow_merge_revisions
SELECT repo, pr, sha, family FROM pipeline_events
WHERE kind IN ('pr.merged','queue.landed') AND repo IS NOT NULL AND pr IS NOT NULL AND length(sha)=40;
CREATE TABLE flow_delivery_receipts (
    event_seq INTEGER NOT NULL, lot_id TEXT NOT NULL, repo TEXT NOT NULL,
    revision TEXT NOT NULL, release TEXT, kind TEXT NOT NULL, ts_ms INTEGER NOT NULL,
    PRIMARY KEY(event_seq, lot_id)
);
CREATE TABLE flow_delivery_reads (event_seq INTEGER PRIMARY KEY);
-- Shift and Stage had synthetic starts on arrival. No producer measured them.
UPDATE flow_ledger SET started_ms=NULL WHERE station IN ('shift','stage') AND started_ms=entered_ms;
