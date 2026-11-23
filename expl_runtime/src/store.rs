//! An explorer's SQLite database: one file holding the application's own
//! state beside the framework's event log, deliveries and assignments.
//!
//! Every process opens its own connection. WAL mode lets readers run
//! beside the single writer, and the busy timeout makes concurrent writers
//! wait instead of failing. A database managed by a daemon may be reached
//! from several hosts, where the WAL index cannot be shared, so it uses
//! rollback journaling instead.

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::event::NOW;

/// How long a writer waits for another before reporting a busy database.
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// File beside the database that marks it as managed by a daemon.
pub const CLUSTER_MARKER: &str = "cluster";

/// Opening the database failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failure.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// SQLite could not apply the required journal mode.
    #[error("database requires {expected} journaling, got {actual}")]
    JournalMode {
        /// Required journal mode.
        expected: &'static str,
        /// Actual mode reported by SQLite.
        actual: String,
    },
}

/// Open the database at `path`, creating the file as needed. Tables are
/// the caller's to create: [`initialize`] for the framework's, and the
/// application's own migrations for the rest.
///
/// # Errors
///
/// [`StoreError`] on SQLite failure or an unavailable journal mode.
pub fn open(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let shared = path
        .parent()
        .is_some_and(|dir| dir.join(CLUSTER_MARKER).is_file());
    journal(&conn, if shared { "DELETE" } else { "WAL" })?;
    if shared {
        conn.pragma_update(None, "synchronous", "FULL")?;
    }
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(conn)
}

/// Use rollback journaling on clustered storage. The WAL index is only
/// shared by processes on one host. Switching modes needs exclusive access;
/// SQLite can report a busy database at once rather than waiting, so a process
/// opening it alongside another retries for as long as the busy timeout.
fn journal(conn: &Connection, mode: &'static str) -> Result<(), StoreError> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn
            .pragma_update_and_check(None, "journal_mode", mode, |row| row.get::<_, String>(0))
        {
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(actual) if !actual.eq_ignore_ascii_case(mode) => {
                return Err(StoreError::JournalMode {
                    expected: mode,
                    actual,
                });
            }
            result => return result.map(drop).map_err(StoreError::from),
        }
    }
}

/// Create the framework's tables in a fresh explorer's database: the event
/// log, each consumer's deliveries, and the recipients recorded for routed
/// groups. Existing tables are left as they are.
///
/// The columns `agent` and `role` hold consumer and group names; they keep
/// the names existing databases were created with.
///
/// # Errors
///
/// SQLite failure.
pub fn initialize(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS events (
            id          INTEGER PRIMARY KEY,
            kind        TEXT NOT NULL,
            subject     INTEGER,
            payload     TEXT,
            priority    INTEGER NOT NULL DEFAULT 0,
            created_at  TEXT NOT NULL DEFAULT ({NOW}),
            traceparent TEXT,
            tracestate  TEXT
         );
         CREATE INDEX IF NOT EXISTS events_kind ON events (kind, id);

         CREATE TABLE IF NOT EXISTS deliveries (
            agent        TEXT NOT NULL,
            event_id     INTEGER NOT NULL REFERENCES events (id),
            status       TEXT NOT NULL
                         CHECK (status IN ('pending', 'leased', 'done', 'failed')),
            process_id   INTEGER,
            attempts     INTEGER NOT NULL DEFAULT 0,
            leased_until INTEGER,
            lease_token  TEXT,
            note         TEXT,
            updated_at   TEXT NOT NULL DEFAULT ({NOW}),
            PRIMARY KEY (agent, event_id)
         );
         CREATE INDEX IF NOT EXISTS deliveries_lease
            ON deliveries (agent, status, leased_until);
         CREATE UNIQUE INDEX IF NOT EXISTS deliveries_token
            ON deliveries (lease_token) WHERE lease_token IS NOT NULL;

         CREATE TABLE IF NOT EXISTS assignments (
            role       TEXT NOT NULL,
            event_id   INTEGER NOT NULL REFERENCES events (id),
            agent      TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT ({NOW}),
            PRIMARY KEY (role, event_id)
         );"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal_mode(conn: &Connection) -> String {
        conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode")
    }

    #[test]
    fn initializing_twice_keeps_what_is_there() {
        let dir = tempfile::tempdir().expect("dir");
        let conn = open(&dir.path().join("explorer.db")).expect("open");
        initialize(&conn).expect("tables");
        conn.execute(
            "INSERT INTO events (kind, subject) VALUES ('scenario.added', 7)",
            [],
        )
        .expect("event");
        initialize(&conn).expect("again");
        let (kind, priority): (String, i64) = conn
            .query_row("SELECT kind, priority FROM events", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("kept");
        assert_eq!((kind.as_str(), priority), ("scenario.added", 0));
    }

    #[test]
    fn a_database_managed_by_a_daemon_uses_rollback_journaling() {
        let local = tempfile::tempdir().expect("dir");
        let conn = open(&local.path().join("explorer.db")).expect("open");
        assert_eq!(journal_mode(&conn), "wal");

        let managed = tempfile::tempdir().expect("dir");
        std::fs::write(managed.path().join(CLUSTER_MARKER), "/shared/cluster").expect("marker");
        let conn = open(&managed.path().join("explorer.db")).expect("open");
        assert_eq!(journal_mode(&conn), "delete");
        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("synchronous");
        assert_eq!(synchronous, 2, "FULL");
        let foreign_keys: bool = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("foreign keys");
        assert!(foreign_keys);
    }
}
