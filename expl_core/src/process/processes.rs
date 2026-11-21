//! [`Processes`] — the process table in the project database.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::process::{ProcessId, ProcessRecord, ProcessState, ProcessStatus};
use crate::store::{self, EPOCH, NOW, StoreError};

/// How often a running process beats.
pub const HEARTBEAT_SECONDS: u64 = 2;
/// A running process that has not beaten for this long is dead.
pub const STALE_SECONDS: i64 = 15;

const COLUMNS: &str = "id, name, agent, pid, status, detached, log_path, stop_requested, \
                       activity, exit_reason, created_at, finished_at, \
                       COALESCE(CAST(strftime('%s','now') AS INTEGER) - heartbeat_at, 0)";

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
    let state = match status {
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

    /// Register a process of `agent`, named `name` or `<agent>-<id>`.
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
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO processes (name, agent, status, detached, log_path) \
             VALUES (?1, ?2, 'created', ?3, '')",
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

    /// The OS process `pid` adopted the record and is running.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn start(&self, id: ProcessId, pid: u32) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            &format!(
                "UPDATE processes SET status = 'running', pid = ?2, heartbeat_at = {EPOCH} \
                 WHERE id = ?1"
            ),
            params![id.0, pid],
        )?;
        Ok(())
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
