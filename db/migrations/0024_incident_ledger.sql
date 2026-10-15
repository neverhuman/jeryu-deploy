-- 0024_incident_ledger: incidents and the changes blamed for them, kept for good.
--
-- Change failure rate, escaped-to-prod defects and recovery time are read from
-- here (docs/pipeline-events.md, "Incidents"). The log of pipeline events is
-- pruned, so the incident events are folded into two tables as they are
-- stored, in the same transaction as the event, and never pruned.
--
-- incident_ledger holds one row per incident. A row may be created by an event
-- that is not `incident.opened` (a restore reported before the opening, or a
-- cause before either): detected_ms is then NULL until the opening arrives.
-- incident_causes holds one row per (incident, change lot key); an incident
-- with no row there has a cause that is not known yet, and a change with a row
-- there is failed.
--
-- Store: <data_dir>/shift.sqlite, written by web/incidents.rs through
-- web/pipeline/store.rs. Rollback: stop the server, DROP TABLE incident_causes,
-- DROP TABLE incident_ledger and delete version 24 from
-- shift_schema_migrations. Two new tables over no rows; no existing row is
-- touched; an older binary never reads them.

CREATE TABLE IF NOT EXISTS incident_ledger (
    incident_id      TEXT    PRIMARY KEY,
    family           TEXT,
    service          TEXT,
    severity         TEXT,
    summary          TEXT,
    detected_ms      INTEGER,
    restored_ms      INTEGER,
    restoring_change TEXT,
    closed_ms        INTEGER,
    reporter         TEXT    NOT NULL,
    first_seq        INTEGER NOT NULL,
    last_seq         INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS incident_ledger_family
    ON incident_ledger (family, detected_ms);

CREATE TABLE IF NOT EXISTS incident_causes (
    incident_id TEXT    NOT NULL,
    change_key  TEXT    NOT NULL,
    reported_ms INTEGER NOT NULL,
    PRIMARY KEY (incident_id, change_key)
);

CREATE INDEX IF NOT EXISTS incident_causes_change
    ON incident_causes (change_key);
