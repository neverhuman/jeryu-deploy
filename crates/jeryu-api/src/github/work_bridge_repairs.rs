//! The pending Work-mirror repair queue.
//!
//! A GitHub-compatible issue write that cannot reach the Work Tracker store
//! still answers the caller, and records what a human has to repair. That
//! queue used to live only in the router, so every restart dropped the pending
//! repairs. It is now durable: with a store attached (the web server attaches
//! `<data_dir>/shift.sqlite`, schema from `db/migrations/0004_work_bridge_repairs.sql`)
//! the queue reloads on startup, a repeated failure refreshes the one row that
//! names it, and a later bridge write that succeeds for the same issue clears
//! it. With no store attached — unit tests, embedding callers — the queue is
//! in-process only, exactly as before.

use std::sync::{Arc, Mutex};

use super::WorkBridgeRepair;

/// Pending repairs, in memory and (when attached) in the durable store.
///
/// The in-memory vector is the read path: it is seeded from the store on
/// attach and stays in step with every write, so listing repairs never touches
/// sqlite.
#[derive(Clone, Debug, Default)]
pub(crate) struct WorkBridgeRepairQueue {
    pending: Arc<Mutex<Vec<WorkBridgeRepair>>>,
    #[cfg(feature = "web")]
    store: Option<store::WorkBridgeRepairStore>,
}

impl WorkBridgeRepairQueue {
    /// Attaches the durable store at `path` and seeds the queue with the
    /// repairs it already holds. Errors carry the sqlite failure so the caller
    /// can refuse to start rather than run with a queue that silently forgets.
    #[cfg(feature = "web")]
    pub(crate) fn with_store(path: &std::path::Path) -> Result<Self, String> {
        let store = store::WorkBridgeRepairStore::open(path)?;
        let pending = store.load()?;
        Ok(Self {
            pending: Arc::new(Mutex::new(pending)),
            store: Some(store),
        })
    }

    /// Every repair still waiting for a human, oldest first.
    pub(crate) fn pending(&self) -> Vec<WorkBridgeRepair> {
        self.lock().clone()
    }

    /// Records one repair. A repeat of the same failure for the same issue
    /// replaces the entry that names it instead of appending, so a bridge that
    /// keeps failing cannot grow the queue without bound.
    pub(crate) fn record(&self, repair: WorkBridgeRepair) {
        {
            let mut pending = self.lock();
            match pending.iter_mut().find(|held| same_repair(held, &repair)) {
                Some(held) => *held = repair.clone(),
                None => pending.push(repair.clone()),
            }
        }
        #[cfg(feature = "web")]
        if let Some(store) = &self.store {
            store.record(&repair);
        }
    }

    /// Drops every repair filed against `owner/repo#issue_number`, called when
    /// a later bridge write for that issue reaches the Work store: the repair
    /// has been applied, so it must not outlive the failure it describes.
    pub(crate) fn resolve(&self, owner: &str, repo: &str, issue_number: u64) {
        let removed = {
            let mut pending = self.lock();
            let before = pending.len();
            pending.retain(|repair| {
                !(repair.owner == owner
                    && repair.repo == repo
                    && repair.issue_number == issue_number)
            });
            before != pending.len()
        };
        if !removed {
            return;
        }
        #[cfg(feature = "web")]
        if let Some(store) = &self.store {
            store.resolve(owner, repo, issue_number);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<WorkBridgeRepair>> {
        self.pending.lock().expect("work bridge repair queue lock")
    }
}

/// Two repairs are the same pending item when they name the same failure of
/// the same bridge operation on the same issue.
fn same_repair(left: &WorkBridgeRepair, right: &WorkBridgeRepair) -> bool {
    left.owner == right.owner
        && left.repo == right.repo
        && left.issue_number == right.issue_number
        && left.operation == right.operation
        && left.code == right.code
}

#[cfg(feature = "web")]
mod store {
    //! The sqlite half of the queue, in `<data_dir>/shift.sqlite`.
    //!
    //! Writes are best-effort on purpose: the caller is already answering a
    //! degraded issue write, and a store that cannot be written must not turn
    //! that 201 into a 500. A failed write is reported to the log and the
    //! in-memory queue still holds the repair for this process's lifetime.

    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use rusqlite::{Connection, Row, params, types::Type};

    use super::WorkBridgeRepair;

    const COLUMNS: &str = "owner, repo, issue_number, operation, code, work_key, reason,
         common_fixes, docs_url, repair_hint";

    #[derive(Clone)]
    pub(super) struct WorkBridgeRepairStore {
        inner: Arc<Mutex<Connection>>,
    }

    impl std::fmt::Debug for WorkBridgeRepairStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("WorkBridgeRepairStore")
        }
    }

    impl WorkBridgeRepairStore {
        pub(super) fn open(path: &Path) -> Result<Self, String> {
            let conn = Connection::open(path).map_err(|err| err.to_string())?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(|err| err.to_string())?;
            crate::web::shift::migrate_shift_store(&conn)?;
            Ok(Self {
                inner: Arc::new(Mutex::new(conn)),
            })
        }

        pub(super) fn load(&self) -> Result<Vec<WorkBridgeRepair>, String> {
            let conn = self.lock();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM work_bridge_repairs ORDER BY recorded_ms, rowid"
                ))
                .map_err(|err| err.to_string())?;
            let rows = stmt
                .query_map([], repair_from_row)
                .map_err(|err| err.to_string())?;
            let mut repairs = Vec::new();
            for row in rows {
                match row {
                    Ok(repair) => repairs.push(repair),
                    // One unreadable row must not discard every other pending repair.
                    Err(err) => report("load", &err.to_string()),
                }
            }
            Ok(repairs)
        }

        pub(super) fn record(&self, repair: &WorkBridgeRepair) {
            let common_fixes = match serde_json::to_string(&repair.common_fixes) {
                Ok(json) => json,
                Err(err) => {
                    report("record", &err.to_string());
                    return;
                }
            };
            let conn = self.lock();
            let written = conn.execute(
                &format!(
                    "INSERT INTO work_bridge_repairs ({COLUMNS}, recorded_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                     ON CONFLICT (owner, repo, issue_number, operation, code) DO UPDATE SET
                       work_key = excluded.work_key,
                       reason = excluded.reason,
                       common_fixes = excluded.common_fixes,
                       docs_url = excluded.docs_url,
                       repair_hint = excluded.repair_hint,
                       recorded_ms = excluded.recorded_ms"
                ),
                params![
                    repair.owner,
                    repair.repo,
                    repair.issue_number,
                    repair.operation,
                    repair.code,
                    repair.work_key,
                    repair.reason,
                    common_fixes,
                    repair.docs_url,
                    repair.repair_hint,
                    chrono::Utc::now().timestamp_millis(),
                ],
            );
            if let Err(err) = written {
                report("record", &err.to_string());
            }
        }

        pub(super) fn resolve(&self, owner: &str, repo: &str, issue_number: u64) {
            let conn = self.lock();
            let removed = conn.execute(
                "DELETE FROM work_bridge_repairs
                  WHERE owner = ?1 AND repo = ?2 AND issue_number = ?3",
                params![owner, repo, issue_number],
            );
            if let Err(err) = removed {
                report("resolve", &err.to_string());
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
            self.inner
                .lock()
                .expect("work bridge repair store mutex poisoned")
        }
    }

    fn report(operation: &str, error: &str) {
        eprintln!("work bridge repair store {operation} failed: {error}");
    }

    fn repair_from_row(row: &Row<'_>) -> rusqlite::Result<WorkBridgeRepair> {
        let common_fixes: String = row.get(7)?;
        Ok(WorkBridgeRepair {
            owner: row.get(0)?,
            repo: row.get(1)?,
            issue_number: row.get(2)?,
            operation: row.get(3)?,
            code: row.get(4)?,
            work_key: row.get(5)?,
            reason: row.get(6)?,
            common_fixes: serde_json::from_str(&common_fixes).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(7, Type::Text, Box::new(error))
            })?,
            docs_url: row.get(8)?,
            repair_hint: row.get(9)?,
        })
    }
}
