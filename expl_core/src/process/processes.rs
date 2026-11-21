//! [`Processes`] — the process table in the project database.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::process::{ProcessId, ProcessRecord, ProcessState, ProcessStatus};
use crate::store::{self, EPOCH, NOW, StoreError};

/// How often a running process beats.
pub const HEARTBEAT_SECONDS: u64 = 2;
/// A running process that has not beaten for this long is dead.
pub const STALE_SECONDS: i64 = 15;
/// A created record must be adopted within this long, or it is abandoned.
pub const STARTUP_SECONDS: i64 = 30;

/// Created and past its startup deadline (also without one).
const EXPIRED: &str =
    "status = 'created' AND COALESCE(start_by, 0) < CAST(strftime('%s','now') AS INTEGER)";

const COLUMNS: &str = "id, name, agent, pid, status, detached, log_path, stop_requested, \
                       activity, exit_reason, created_at, finished_at, \
                       COALESCE(CAST(strftime('%s','now') AS INTEGER) - heartbeat_at, 0), \
                       strftime('%Y-%m-%dT%H:%M:%SZ', start_by, 'unixepoch'), \
                       COALESCE(start_by, 0) < CAST(strftime('%s','now') AS INTEGER)";

/// See the [module docs](crate::process).
pub struct Processes {
    conn: Connection,
    logs: PathBuf,
}

impl std::fmt::Debug for Processes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Processes").finish_non_exhaustive()
    }
}

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<ProcessRecord> {
    let status: String = row.get(4)?;
    let status: ProcessStatus = status.parse().map_err(|reason: String| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(reason)),
        )
    })?;
    let silence: i64 = row.get(12)?;
    let overdue: bool = row.get(14)?;
    let state = match status {
        ProcessStatus::Created if overdue => ProcessState::Dead,
        ProcessStatus::Created => ProcessState::Created,
        ProcessStatus::Running if silence > STALE_SECONDS => ProcessState::Dead,
        ProcessStatus::Running => ProcessState::Running,
        ProcessStatus::Exited => ProcessState::Exited,
    };
    Ok(ProcessRecord {
        id: ProcessId(row.get(0)?),
        name: row.get(1)?,
        agent: row.get(2)?,
        pid: row.get(3)?,
        state,
        detached: row.get(5)?,
        log_path: row.get(6)?,
        stop_requested: row.get(7)?,
        activity: row.get(8)?,
        exit_reason: row.get(9)?,
        created_at: row.get(10)?,
        start_by: row.get(13)?,
        finished_at: row.get(11)?,
    })
}

impl Processes {
    /// The process table of the database at `path`; logs go to `logs`.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the database cannot be opened.
    pub fn open(path: &Path, logs: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            conn: store::open(path)?,
            logs: logs.to_path_buf(),
        })
    }

    /// Register a process of `agent`, named `name` or `<agent>-<id>`, to
    /// be adopted within [`STARTUP_SECONDS`].
    ///
    /// # Errors
    ///
    /// SQLite failure, including a duplicate name.
    pub fn create(
        &mut self,
        agent: &str,
        name: Option<&str>,
        detached: bool,
    ) -> Result<ProcessRecord, rusqlite::Error> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            &format!(
                "INSERT INTO processes (name, agent, status, detached, log_path, start_by) \
                 VALUES (?1, ?2, 'created', ?3, '', {EPOCH} + {STARTUP_SECONDS})"
            ),
            params![
                name.map_or_else(
                    || format!("{agent}-pending-{}", std::process::id()),
                    str::to_owned
                ),
                agent,
                detached
            ],
        )?;
        let id = tx.last_insert_rowid();
        let name = name.map_or_else(|| format!("{agent}-{id}"), str::to_owned);
        let log = self.logs.join(format!("{id}.log"));
        tx.execute(
            "UPDATE processes SET name = ?2, log_path = ?3 WHERE id = ?1",
            params![id, name, log.display().to_string()],
        )?;
        tx.commit()?;
        self.get(ProcessId(id))
    }

    /// The record with `id`.
    ///
    /// # Errors
    ///
    /// SQLite failure, or no such record.
    pub fn get(&self, id: ProcessId) -> Result<ProcessRecord, rusqlite::Error> {
        self.conn.query_row(
            &format!("SELECT {COLUMNS} FROM processes WHERE id = ?1"),
            [id.0],
            record_from_row,
        )
    }

    /// Resolve an operator target: a process id or name.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn find(&self, target: &str) -> Result<Option<ProcessRecord>, rusqlite::Error> {
        if let Ok(id) = target.parse::<i64>()
            && let Some(record) = self
                .conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM processes WHERE id = ?1"),
                    [id],
                    record_from_row,
                )
                .optional()?
        {
            return Ok(Some(record));
        }
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM processes WHERE name = ?1"),
                [target],
                record_from_row,
            )
            .optional()
    }

    /// All records (live only unless `all`), oldest first.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn list(&self, all: bool) -> Result<Vec<ProcessRecord>, rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM processes ORDER BY id"))?;
        let rows = stmt.query_map([], record_from_row)?;
        let mut records = Vec::new();
        for row in rows {
            let record = row?;
            if all || record.is_live() {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// The OS process `pid` adopts the record and is running. Adoption is
    /// one conditional write: it succeeds only while the record is still
    /// created and within its startup deadline, so a record is adopted at
    /// most once and never after it was abandoned. Returns whether `pid`
    /// adopted it.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn start(&self, id: ProcessId, pid: u32) -> Result<bool, rusqlite::Error> {
        let adopted = self.conn.execute(
            &format!(
                "UPDATE processes SET status = 'running', pid = ?2, heartbeat_at = {EPOCH} \
                 WHERE id = ?1 AND status = 'created' AND start_by >= {EPOCH}"
            ),
            params![id.0, pid],
        )?;
        Ok(adopted == 1)
    }

    /// Mark every created record past its startup deadline exited, so the
    /// replica slot an interrupted launch held is free again. Returns the
    /// records abandoned.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn abandon_unstarted(&self) -> Result<Vec<ProcessRecord>, rusqlite::Error> {
        let ids = {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT id FROM processes WHERE {EXPIRED} ORDER BY id"
            ))?;
            stmt.query_map([], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let reason = format!("abandoned: not adopted within {STARTUP_SECONDS}s");
        let mut abandoned = Vec::new();
        for id in ids {
            // Re-checked in the write: the record may have been adopted or
            // abandoned by someone else since it was read.
            let changed = self.conn.execute(
                &format!(
                    "UPDATE processes SET status = 'exited', activity = NULL, exit_reason = ?2, \
                     finished_at = {NOW} WHERE id = ?1 AND {EXPIRED}"
                ),
                params![id, reason],
            )?;
            if changed == 1 {
                abandoned.push(self.get(ProcessId(id))?);
            }
        }
        Ok(abandoned)
    }

    /// Beat; returns whether a stop was requested.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn heartbeat(&self, id: ProcessId) -> Result<bool, rusqlite::Error> {
        self.conn.execute(
            &format!("UPDATE processes SET heartbeat_at = {EPOCH} WHERE id = ?1"),
            [id.0],
        )?;
        self.conn.query_row(
            "SELECT stop_requested FROM processes WHERE id = ?1",
            [id.0],
            |r| r.get(0),
        )
    }

    /// Record what the process is doing.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn set_activity(
        &self,
        id: ProcessId,
        activity: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE processes SET activity = ?2 WHERE id = ?1",
            params![id.0, activity],
        )?;
        Ok(())
    }

    /// Ask the process to stop at the next opportunity.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn request_stop(&self, id: ProcessId) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE processes SET stop_requested = 1 WHERE id = ?1",
            [id.0],
        )?;
        Ok(())
    }

    /// The process finished.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn exit(&self, id: ProcessId, reason: &str) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            &format!(
                "UPDATE processes SET status = 'exited', activity = NULL, exit_reason = ?2, \
                 finished_at = {NOW} WHERE id = ?1"
            ),
            params![id.0, reason],
        )?;
        Ok(())
    }

    /// Forget a record.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn remove(&self, id: ProcessId) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE agent_runs SET process_id = NULL WHERE process_id = ?1",
            [id.0],
        )?;
        self.conn
            .execute("DELETE FROM processes WHERE id = ?1", [id.0])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> (tempfile::TempDir, Processes) {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = Processes::open(&dir.path().join("expl.db"), dir.path()).expect("open");
        (dir, table)
    }

    fn age(table: &Processes, id: ProcessId) {
        table
            .conn
            .execute(
                &format!("UPDATE processes SET start_by = {EPOCH} - 1 WHERE id = ?1"),
                [id.0],
            )
            .expect("age");
    }

    #[test]
    fn a_created_record_is_adopted_once() {
        let (_dir, mut table) = table();
        let record = table.create("explorer", None, true).expect("create");
        assert_eq!(record.state, ProcessState::Created);
        assert!(record.start_by.is_some());
        assert!(table.start(record.id, 1).expect("start"));
        assert!(!table.start(record.id, 2).expect("start"));
        assert_eq!(table.get(record.id).expect("get").pid, Some(1));
        assert!(table.abandon_unstarted().expect("abandon").is_empty());
    }

    #[test]
    fn an_overdue_record_is_dead_abandoned_and_unadoptable() {
        let (_dir, mut table) = table();
        let overdue = table.create("explorer", None, true).expect("create");
        let fresh = table.create("explorer", None, true).expect("create");
        age(&table, overdue.id);
        let record = table.get(overdue.id).expect("get");
        assert_eq!(record.state, ProcessState::Dead);
        assert!(!record.is_live());

        let abandoned = table.abandon_unstarted().expect("abandon");
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].id, overdue.id);
        assert_eq!(abandoned[0].state, ProcessState::Exited);
        assert!(!table.start(overdue.id, 1).expect("start"));
        assert_eq!(table.get(overdue.id).expect("get").pid, None);
        assert_eq!(
            table.get(fresh.id).expect("get").state,
            ProcessState::Created
        );
    }

    #[test]
    fn an_overdue_record_cannot_be_adopted_before_it_is_abandoned() {
        let (_dir, mut table) = table();
        let record = table.create("explorer", None, true).expect("create");
        age(&table, record.id);
        assert!(!table.start(record.id, 1).expect("start"));
        assert_eq!(table.get(record.id).expect("get").state, ProcessState::Dead);
    }
}
