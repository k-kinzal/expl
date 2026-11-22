//! The project's SQLite database: one file, `.expl/expl.db`, shared by the
//! [`Graph`](crate::graph::Graph), the [`Inbox`](crate::event::Inbox) (with
//! its weighted assignments), and the
//! [`Processes`](crate::process::Processes) table.
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
    r"
-- Every lease carries its own token; only its holder may renew or settle
-- it. Leases taken before tokens existed have none and simply expire.
ALTER TABLE deliveries ADD COLUMN lease_token TEXT;
",
    r"
-- A created record must be adopted by its startup deadline (Unix seconds),
-- so an interrupted launch cannot hold a replica slot forever. Records
-- created before deadlines existed have none and count as abandoned.
ALTER TABLE processes ADD COLUMN start_by INTEGER;
",
    r"
-- Agents sharing a role by weight split its events: the first process to
-- route an event records the one agent it goes to, and every process obeys
-- that record, whatever weights it loaded and however often it restarts.
CREATE TABLE assignments (
    role       TEXT NOT NULL,
    event_id   INTEGER NOT NULL REFERENCES events (id),
    agent      TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (role, event_id)
);
",
    r"
-- Investigation steps change nothing: they answer a question about the code
-- or the world and record a finding (yes, no, or unknown) with its evidence
-- in `outcome`. Their `head_commit` is the code they were about; they have
-- no branch and never reach the trunk.
ALTER TABLE nodes ADD COLUMN investigation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN answer TEXT CHECK (answer IN ('yes', 'no', 'unknown'));
",
    r"
-- A step proposed as a new hypothesis grown from a failed step records that
-- step, so the graph shows which failure each alternative came from. It is
-- not an edge: the failed step is not built on.
ALTER TABLE nodes ADD COLUMN derived_from INTEGER REFERENCES nodes (id);
",
    r"
-- The exploration agent that proposed a step (NULL: added by a human, or
-- not known). Steps proposed before it was recorded are attributed to the
-- exploration run that finished within the 5 seconds before them, since an
-- exploration records its proposals as soon as its agent returns.
ALTER TABLE nodes ADD COLUMN proposed_by TEXT;
UPDATE nodes SET proposed_by = (
    SELECT r.agent FROM agent_runs r
    WHERE r.role = 'exploration' AND r.status = 'succeeded'
      AND r.finished_at <= nodes.created_at
      AND r.finished_at >= strftime('%Y-%m-%dT%H:%M:%fZ', nodes.created_at, '-5 seconds')
    ORDER BY r.finished_at DESC LIMIT 1
) WHERE kind = 'step';
",
    r"
-- What evaluation concluded about a step it judged, before (and apart
-- from) the goals: whether it is done, abandoned, or to be continued, the
-- hypothesis it stood for, what was found, and what that changes. A step
-- continued is put back to run again, starting from `resume_commit` (what
-- the attempt left, for a change step) with the conclusion as guidance;
-- `reopened` says whether it was.
CREATE TABLE conclusions (
    id            INTEGER PRIMARY KEY,
    node_id       INTEGER NOT NULL REFERENCES nodes (id),
    decision      TEXT NOT NULL CHECK (decision IN ('done', 'abandoned', 'continue')),
    hypothesis    TEXT NOT NULL,
    finding       TEXT NOT NULL,
    consequence   TEXT NOT NULL,
    resume_commit TEXT,
    reopened      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX conclusions_node ON conclusions (node_id);
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
    wal(&conn)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Switch `conn` to WAL. Switching a new database needs it to itself, and
/// SQLite reports a busy one at once rather than waiting, so a process
/// opening it alongside another retries for as long as the busy timeout.
fn wal(conn: &Connection) -> Result<(), StoreError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return Ok(result?),
        }
    }
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
    if current == supported {
        return Ok(());
    }
    // Several processes may open an old database at once: each migration
    // takes the write lock first and reads the version under it, so only
    // one of them applies it.
    loop {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if current > supported {
            return Err(StoreError::SchemaTooNew {
                found: current,
                supported,
            });
        }
        let Some(sql) = MIGRATIONS.get(current as usize) else {
            return Ok(());
        };
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", current + 1)?;
        tx.commit()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processes_opening_a_new_database_at_once_migrate_it_once() {
        for _ in 0..5 {
            let dir = tempfile::tempdir().expect("dir");
            let path = dir.path().join("expl.db");
            let start = std::sync::Barrier::new(4);
            std::thread::scope(|scope| {
                let openers: Vec<_> = (0..4)
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            open(&path).map(|_| ())
                        })
                    })
                    .collect();
                for opener in openers {
                    opener.join().expect("thread").expect("open");
                }
            });
            let version: i64 = open(&path)
                .expect("open")
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .expect("version");
            assert_eq!(version, MIGRATIONS.len() as i64);
        }
    }
}
