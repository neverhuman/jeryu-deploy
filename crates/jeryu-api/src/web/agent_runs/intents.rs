//! The durable intent of every agent run the forge acknowledges.
//!
//! A run's live state (its TTY ring, its control channel, its driver thread) is
//! process-local by nature, but the fact that the forge accepted a run, and the
//! id it answered with, must not be. The intent of each run therefore lands in
//! `<data_dir>/shift.sqlite` (`agent_run_intents`,
//! `db/migrations/0008_agent_run_intents.sql`) BEFORE the start answer leaves
//! the handler, and the run id comes from the durable counter in
//! `agent_run_id_sequence` rather than a counter that restarts with the
//! process. A caller that holds a run id therefore holds an id that exists on
//! disk and that no later run is ever given again.
//!
//! When the store cannot be written the start is refused: a caller is never
//! told a run was accepted when nothing recorded it.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, params};

use crate::web::shift::migrate_shift_store;

/// How long a recorded intent is kept, matching the rest of the shift store.
const RETENTION_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// What the forge records about a run before it acknowledges it.
#[derive(Clone, Debug)]
pub(in crate::web) struct AgentRunIntent {
    /// The id to record, when the caller named one. `None` takes the next id
    /// from the durable sequence.
    pub run_id: Option<String>,
    /// Which surface acknowledged the run: `agent_run`, `session` or `shell`.
    pub kind: &'static str,
    /// Owning repository `owner/name`, if the run has one.
    pub repo: Option<String>,
    pub program: String,
    pub args: Vec<String>,
    pub workspace: Option<String>,
}

#[derive(Clone)]
pub(in crate::web) struct AgentRunIntentStore {
    inner: Arc<Mutex<Connection>>,
}

impl AgentRunIntentStore {
    /// Open the intents in `path` (the shift store).
    pub(in crate::web) fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|err| err.to_string())?;
        migrate_shift_store(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// A store of its own, for a test that wants no file.
    #[cfg(test)]
    pub(in crate::web) fn in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("open an in-memory agent run intent store");
        migrate_shift_store(&conn).expect("migrate an in-memory agent run intent store");
        Self {
            inner: Arc::new(Mutex::new(conn)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take the next run id from the durable sequence without recording an
    /// intent for it yet. The session route reads this to find an id whose
    /// session branch does not exist, then records the intent under the id it
    /// settles on. An error means no id was handed out.
    pub(in crate::web) fn allocate_id(&self) -> Result<String, String> {
        let conn = self.lock();
        next_run_id(&conn)
    }

    /// Record one run's intent and return the run id it is recorded under. The
    /// id and the intent land in one transaction, so an id is only ever handed
    /// out together with the row that describes what it was handed out for.
    /// An error means nothing was recorded and the run must not be started.
    pub(in crate::web) fn record(
        &self,
        intent: &AgentRunIntent,
        now_ms: i64,
    ) -> Result<String, String> {
        let args_json = serde_json::to_string(&intent.args).map_err(|err| err.to_string())?;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(|err| err.to_string())?;
        let run_id = match intent.run_id.clone() {
            Some(run_id) => run_id,
            None => next_run_id(&tx)?,
        };
        tx.execute(
            "INSERT INTO agent_run_intents
               (run_id, kind, repo, program, args_json, workspace, recorded_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(run_id) DO UPDATE SET
               kind = excluded.kind,
               repo = excluded.repo,
               program = excluded.program,
               args_json = excluded.args_json,
               workspace = excluded.workspace,
               recorded_ms = excluded.recorded_ms",
            params![
                run_id,
                intent.kind,
                intent.repo,
                intent.program,
                args_json,
                intent.workspace,
                now_ms,
            ],
        )
        .map_err(|err| err.to_string())?;
        tx.execute(
            "DELETE FROM agent_run_intents WHERE recorded_ms < ?1",
            params![now_ms - RETENTION_MS],
        )
        .map_err(|err| err.to_string())?;
        tx.commit().map_err(|err| err.to_string())?;
        Ok(run_id)
    }

    /// True when `run_id` is recorded on disk. The route tests read this to
    /// prove an acknowledged id was durable at the moment it was answered.
    #[cfg(test)]
    pub(in crate::web) fn is_recorded(&self, run_id: &str) -> bool {
        let conn = self.lock();
        conn.query_row(
            "SELECT 1 FROM agent_run_intents WHERE run_id = ?1",
            params![run_id],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Drop the intents table, so the next `record` fails the way a full or
    /// unwritable disk does. The negative tests use this to drive the real
    /// start path into its persistence-failure branch.
    #[cfg(test)]
    pub(in crate::web) fn break_for_test(&self) {
        let conn = self.lock();
        conn.execute_batch("DROP TABLE agent_run_intents")
            .expect("drop the agent run intents table");
    }
}

/// Advance the durable run-id counter by one and spell the id it handed out.
fn next_run_id(conn: &Connection) -> Result<String, String> {
    let seq: i64 = conn
        .query_row(
            "UPDATE agent_run_id_sequence SET next_seq = next_seq + 1
             WHERE id = 1 RETURNING next_seq - 1",
            [],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    Ok(format!("ar-{seq:06}"))
}
