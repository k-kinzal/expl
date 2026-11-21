//! The project's SQLite database: one file, `.expl/expl.db`, shared by the
//! [`Graph`](crate::graph::Graph), the [`Inbox`](crate::event::Inbox), and
//! the [`Processes`](crate::process::Processes) table.
//!
//! Every expl process opens its own connection. WAL mode lets readers run
//! beside the single writer, and the busy timeout makes concurrent writers
//! wait instead of failing.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

/// Timestamp expression for every `*_at` text column (UTC, RFC 3339).
pub(crate) const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

/// Unix-seconds expression for lease and heartbeat arithmetic.
pub(crate) const EPOCH: &str = "CAST(strftime('%s','now') AS INTEGER)";

/// Opening or migrating the database failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failure.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database was created by a newer expl.
    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Version stored in the database.
        found: i64,
        /// Newest version this build knows.
        supported: i64,
    },
}

/// Ordered migrations; index + 1 is the resulting `user_version`.
const MIGRATIONS: &[&str] = &[
    r"
CREATE TABLE nodes (
    id                  INTEGER PRIMARY KEY,
    kind                TEXT NOT NULL CHECK (kind IN ('origin', 'step', 'goal')),
    status              TEXT NOT NULL,
    title               TEXT NOT NULL,
    objective           TEXT NOT NULL,
    priority            INTEGER NOT NULL DEFAULT 0,
    counter_exploration INTEGER NOT NULL DEFAULT 0,
    branch              TEXT,
    head_commit         TEXT,
    outcome             TEXT,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE UNIQUE INDEX nodes_single_origin ON nodes (kind) WHERE kind = 'origin';

CREATE TABLE edges (
    source_id  INTEGER NOT NULL REFERENCES nodes (id),
    target_id  INTEGER NOT NULL REFERENCES nodes (id),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (source_id, target_id),
    CHECK (source_id <> target_id)
);
CREATE INDEX edges_target ON edges (target_id);

CREATE TABLE plans (
    id         INTEGER PRIMARY KEY,
    summary    TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE planned_paths (
    id        INTEGER PRIMARY KEY,
    plan_id   INTEGER NOT NULL REFERENCES plans (id),
    goal_id   INTEGER NOT NULL REFERENCES nodes (id),
    rationale TEXT NOT NULL
);

CREATE TABLE planned_path_nodes (
    path_id  INTEGER NOT NULL REFERENCES planned_paths (id),
    position INTEGER NOT NULL,
    node_id  INTEGER NOT NULL REFERENCES nodes (id),
    PRIMARY KEY (path_id, position)
);

CREATE TABLE evaluations (
    id         INTEGER PRIMARY KEY,
    node_id    INTEGER NOT NULL REFERENCES nodes (id),
    goal_id    INTEGER NOT NULL REFERENCES nodes (id),
    satisfied  INTEGER NOT NULL,
    score      INTEGER NOT NULL,
    reason     TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX evaluations_node ON evaluations (node_id);

CREATE TABLE events (
    id         INTEGER PRIMARY KEY,
    kind       TEXT NOT NULL,
    node_id    INTEGER REFERENCES nodes (id),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX events_kind ON events (kind, id);

CREATE TABLE deliveries (
    agent        TEXT NOT NULL,
    event_id     INTEGER NOT NULL REFERENCES events (id),
    status       TEXT NOT NULL CHECK (status IN ('pending', 'leased', 'done', 'failed')),
    process_id   INTEGER,
    attempts     INTEGER NOT NULL DEFAULT 0,
    leased_until INTEGER,
    note         TEXT,
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (agent, event_id)
);
CREATE INDEX deliveries_lease ON deliveries (agent, status, leased_until);

CREATE TABLE processes (
    id             INTEGER PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,
    agent          TEXT NOT NULL,
    pid            INTEGER,
    status         TEXT NOT NULL CHECK (status IN ('created', 'running', 'exited')),
    detached       INTEGER NOT NULL,
    log_path       TEXT NOT NULL,
    stop_requested INTEGER NOT NULL DEFAULT 0,
    activity       TEXT,
    exit_reason    TEXT,
    heartbeat_at   INTEGER,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    finished_at    TEXT
);

CREATE TABLE agent_runs (
    id          INTEGER PRIMARY KEY,
    agent       TEXT NOT NULL,
    role        TEXT NOT NULL,
    process_id  INTEGER REFERENCES processes (id),
    event_id    INTEGER REFERENCES events (id),
    node_id     INTEGER REFERENCES nodes (id),
    status      TEXT NOT NULL,
    log_path    TEXT,
    session_id  TEXT,
    error       TEXT,
    started_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    finished_at TEXT
);
CREATE INDEX agent_runs_node ON agent_runs (node_id);
",
    r"
-- The origin moves with the trunk: one active origin, any number of
-- superseded ones. An origin's branch is the trunk branch it was read from.
DROP INDEX nodes_single_origin;
UPDATE nodes SET status = 'active' WHERE kind = 'origin';
UPDATE nodes SET branch = NULL WHERE kind = 'origin' AND branch = 'expl/node-' || id;
CREATE UNIQUE INDEX nodes_current_origin ON nodes (kind)
    WHERE kind = 'origin' AND status = 'active';
",
];

/// Open the database at `path`, creating and migrating it as needed.
///
/// # Errors
///
/// [`StoreError`] on SQLite failure or a too-new schema.
pub fn open(path: &Path) -> Result<Connection, StoreError> {
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(30))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// An in-memory database (tests).
///
/// # Errors
///
/// [`StoreError`] on SQLite failure.
pub fn open_in_memory() -> Result<Connection, StoreError> {
    let mut conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let supported = MIGRATIONS.len() as i64;
    if current > supported {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported,
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}
