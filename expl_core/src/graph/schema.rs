//! Schema migrations, tracked by SQLite's `user_version`.

use rusqlite::Connection;

use crate::graph::GraphError;

/// Timestamp expression used for every `*_at` column (UTC, RFC 3339).
pub(crate) const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

/// Ordered migrations; index + 1 is the resulting `user_version`.
const MIGRATIONS: &[&str] = &[r"
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

CREATE TABLE agent_runs (
    id          INTEGER PRIMARY KEY,
    role        TEXT NOT NULL,
    node_id     INTEGER REFERENCES nodes (id),
    agent       TEXT NOT NULL,
    status      TEXT NOT NULL,
    log_path    TEXT,
    session_id  TEXT,
    error       TEXT,
    started_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    finished_at TEXT
);
CREATE INDEX agent_runs_node ON agent_runs (node_id);
"];

/// Bring the database up to the newest schema.
pub(crate) fn migrate(conn: &mut Connection) -> Result<(), GraphError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let supported = MIGRATIONS.len() as i64;
    if current > supported {
        return Err(GraphError::SchemaTooNew {
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
