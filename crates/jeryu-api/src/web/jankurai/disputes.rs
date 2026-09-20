//! Disputes filed against a jankurai finding, in `<data_dir>/shift.sqlite`.
//!
//! Append-only: a dispute is inserted once and never rewritten, and the
//! identity index makes a retried POST return the first row. The schema comes
//! from the reviewed `db/migrations/0003_jankurai_disputes.sql`, applied by the
//! shift migration runner. The store holds its own connection to the file the
//! heartbeat and pipeline-event stores also use; sqlite serialises the writers
//! and `busy_timeout` covers the overlap.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

use super::super::shift::{migrate_shift_store, rfc3339_ms};

const COLUMNS: &str =
    "id, created_ms, score_id, repo, commit_sha, rule_id, path, line, reason, author";

/// One filed dispute, as the API returns it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Dispute {
    pub id: String,
    pub created_at: String,
    pub score_id: String,
    pub repo: String,
    pub commit_sha: String,
    pub rule_id: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub reason: String,
    pub author: String,
}

/// A dispute about to be stored; `id` and `created_at` are assigned here.
#[derive(Clone, Debug)]
pub(crate) struct NewDispute {
    pub score_id: String,
    pub repo: String,
    pub commit_sha: String,
    pub rule_id: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub reason: String,
    pub author: String,
}

/// What [`DisputeStore::insert`] did.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Filed {
    pub dispute: Dispute,
    /// True when this author had already filed the identical dispute.
    pub duplicate: bool,
}

#[derive(Clone)]
pub(crate) struct DisputeStore {
    inner: Arc<Mutex<Connection>>,
}

impl DisputeStore {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| err.to_string())?;
        migrate_shift_store(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// File one dispute. When the author already filed the identical dispute
    /// nothing is written and that row comes back with `duplicate = true`, so a
    /// retried POST never inflates the dispute rate the overview reports.
    pub(crate) fn insert(&self, dispute: &NewDispute, now_ms: i64) -> Result<Filed, String> {
        let conn = self.inner.lock().expect("jankurai dispute mutex poisoned");
        let existing: Option<Dispute> = conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM jankurai_disputes
                      WHERE score_id = ?1 AND rule_id = ?2
                        AND ifnull(path, '') = ifnull(?3, '')
                        AND ifnull(line, -1) = ifnull(?4, -1)
                        AND author = ?5"
                ),
                params![
                    dispute.score_id,
                    dispute.rule_id,
                    dispute.path,
                    dispute.line,
                    dispute.author
                ],
                dispute_from_row,
            )
            .optional()
            .map_err(|err| err.to_string())?;
        if let Some(dispute) = existing {
            return Ok(Filed {
                dispute,
                duplicate: true,
            });
        }
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO jankurai_disputes
               (id, created_ms, score_id, repo, commit_sha, rule_id, path, line, reason, author)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                now_ms,
                dispute.score_id,
                dispute.repo,
                dispute.commit_sha,
                dispute.rule_id,
                dispute.path,
                dispute.line,
                dispute.reason,
                dispute.author,
            ],
        )
        .map_err(|err| err.to_string())?;
        Ok(Filed {
            dispute: Dispute {
                id,
                created_at: rfc3339_ms(now_ms),
                score_id: dispute.score_id.clone(),
                repo: dispute.repo.clone(),
                commit_sha: dispute.commit_sha.clone(),
                rule_id: dispute.rule_id.clone(),
                path: dispute.path.clone(),
                line: dispute.line,
                reason: dispute.reason.clone(),
                author: dispute.author.clone(),
            },
            duplicate: false,
        })
    }

    /// Every dispute, newest first, optionally narrowed to one score or rule.
    pub(crate) fn list(
        &self,
        score_id: Option<&str>,
        rule_id: Option<&str>,
    ) -> Result<Vec<Dispute>, String> {
        let conn = self.inner.lock().expect("jankurai dispute mutex poisoned");
        let mut statement = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM jankurai_disputes
                  WHERE (?1 IS NULL OR score_id = ?1)
                    AND (?2 IS NULL OR rule_id = ?2)
                  ORDER BY created_ms DESC, id DESC"
            ))
            .map_err(|err| err.to_string())?;
        let rows = statement
            .query_map(params![score_id, rule_id], dispute_from_row)
            .map_err(|err| err.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())
    }
}

fn dispute_from_row(row: &Row<'_>) -> rusqlite::Result<Dispute> {
    let created_ms: i64 = row.get(1)?;
    Ok(Dispute {
        id: row.get(0)?,
        created_at: rfc3339_ms(created_ms),
        score_id: row.get(2)?,
        repo: row.get(3)?,
        commit_sha: row.get(4)?,
        rule_id: row.get(5)?,
        path: row.get(6)?,
        line: row.get(7)?,
        reason: row.get(8)?,
        author: row.get(9)?,
    })
}
