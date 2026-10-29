-- 0028_merge_visit_repair: close the Merge visits left open after their lot
-- landed, once, and record every visit it closed.
--
-- Before c44535 a late gate or review event could open a Merge visit after the
-- lot had already landed, and `first_projection` skips a re-sent landing the
-- ledger already folded in, so nothing ever closed it: the lot waited at Merge
-- forever. Replaying the ledger would fix that but loses every visit older than
-- the 30-day event log, so, like 0020, this repairs the derived rows in place.
--
-- A visit is closed only when all of these hold:
--   - it is the lot's newest open Merge visit;
--   - the event that opened it is still in the log and is not a queue.* event
--     other than queue.landed (queue.enqueued/failed/refused open Merge
--     legitimately; a landing that opened Merge itself, as todo.merged did for
--     the lots seen in production, left a visit nothing would close);
--   - the log holds a landing (queue.landed, pr.merged, todo.merged) naming the
--     lot, by its todo id or by the visit's repository and pull request;
--   - no queue.* event other than queue.landed names the lot after that
--     landing (a re-enqueue, or another pull request of the same todo, keeps
--     its visit open).
-- It is closed as the landing would have closed it: left at the later of the
-- landing and its own entry, direction forward, hold fields cleared, and a
-- `passed` flow_activity row at the landing.
--
-- Store: <data_dir>/shift.sqlite, owned by jeryu-api (web/flow/project.rs
-- projects the ledger), applied once at startup by the shift migration runner
-- (web/shift/heartbeats.rs).
-- Record: flow_merge_visit_repair keeps one row per closed visit with every
-- field it changed, as it was. That is the audit trail (how many, which lots,
-- which landing) and the undo.
-- Rollback: restore each row's prior fields from flow_merge_visit_repair
-- (left_ms NULL, prior direction, source_seq_last, hold fields) and delete the
-- flow_activity rows it lists, or restore the pre-release shift.sqlite backup.
-- An older binary ignores the new table. Delete version 28 from
-- shift_schema_migrations only together with that restore.
-- Lock safety: one transaction over the open Merge visits only (a handful of
-- rows); the event-log lookups use pipeline_events' primary key and indexes.
-- Overlapping binaries: the closed visits are ordinary closed rows, read the
-- same way by an older binary.

CREATE TABLE flow_merge_visit_repair (
    visit_seq                   INTEGER PRIMARY KEY,
    lot_id                      TEXT    NOT NULL,
    landing_seq                 INTEGER NOT NULL,
    left_ms                     INTEGER NOT NULL,
    prior_direction             TEXT    NOT NULL,
    prior_rework_to             TEXT,
    prior_source_seq_last       INTEGER NOT NULL,
    prior_hold_reason           TEXT,
    prior_hold_by               TEXT,
    prior_hold_since_ms         INTEGER,
    prior_hold_sha              TEXT,
    repaired_ms                 INTEGER NOT NULL
);

INSERT INTO flow_merge_visit_repair
SELECT v.seq, v.lot_id, l.seq, MAX(l.ts_ms, v.entered_ms), v.direction, v.rework_to,
       v.source_seq_last, v.hold_reason, v.hold_by, v.hold_since_ms, v.hold_sha,
       CAST(strftime('%s', 'now') AS INTEGER) * 1000
FROM flow_ledger v
JOIN pipeline_events o ON o.seq = v.source_seq_first
JOIN pipeline_events l ON l.seq = (
    SELECT MAX(e.seq) FROM pipeline_events e
    WHERE e.kind IN ('queue.landed', 'pr.merged', 'todo.merged')
      AND ((TRIM(COALESCE(e.todo_id, '')) <> '' AND TRIM(e.todo_id) = v.lot_id)
           OR (e.repo = v.repo AND e.pr = v.pr)))
WHERE v.station = 'merge' AND v.left_ms IS NULL
  AND (o.kind NOT LIKE 'queue.%' OR o.kind = 'queue.landed')
  AND v.seq = (SELECT MAX(x.seq) FROM flow_ledger x
               WHERE x.lot_id = v.lot_id AND x.station = 'merge' AND x.left_ms IS NULL)
  AND NOT EXISTS (
    SELECT 1 FROM pipeline_events q
    WHERE q.seq > l.seq AND q.kind LIKE 'queue.%' AND q.kind <> 'queue.landed'
      AND ((TRIM(COALESCE(q.todo_id, '')) <> '' AND TRIM(q.todo_id) = v.lot_id)
           OR (q.repo = v.repo AND q.pr = v.pr)));

UPDATE flow_ledger
SET left_ms = r.left_ms, direction = 'forward', rework_to = NULL,
    source_seq_last = MAX(flow_ledger.source_seq_last, r.landing_seq),
    hold_reason = NULL, hold_by = NULL, hold_since_ms = NULL, hold_sha = NULL
FROM flow_merge_visit_repair r
WHERE flow_ledger.seq = r.visit_seq;

INSERT OR IGNORE INTO flow_activity
    (event_seq, lot_id, station, state, ts_ms, family, repo, pr, sha, summary, runner, log_url)
SELECT r.landing_seq, r.lot_id, 'merge', 'passed', r.left_ms, v.family, v.repo, v.pr,
       NULL, NULL, NULL, NULL
FROM flow_merge_visit_repair r JOIN flow_ledger v ON v.seq = r.visit_seq;
