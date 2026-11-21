//! [`GraphError`].

use thiserror::Error;

use crate::graph::{NodeId, NodeKind};

/// A graph operation failed.
#[derive(Debug, Error)]
pub enum GraphError {
    /// SQLite failure.
    #[error("graph database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database was created by a newer expl.
    #[error("graph schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Version stored in the database.
        found: i64,
        /// Newest version this build knows.
        supported: i64,
    },
    /// No node with this id.
    #[error("node {0} does not exist")]
    NodeNotFound(NodeId),
    /// The graph has no origin (not initialised).
    #[error("the graph has no origin node")]
    NoOrigin,
    /// An origin already exists.
    #[error("the graph already has an origin node")]
    OriginExists,
    /// An edge would close a cycle.
    #[error("edge {from} -> {target} would create a cycle")]
    Cycle {
        /// Edge source.
        from: NodeId,
        /// Edge target.
        target: NodeId,
    },
    /// An edge endpoint has a kind that cannot take that end.
    #[error("a {kind} node cannot be the {end} of an edge (node {node})")]
    InvalidEndpoint {
        /// Offending node.
        node: NodeId,
        /// Its kind.
        kind: NodeKind,
        /// `source` or `target`.
        end: &'static str,
    },
    /// A stored value does not decode.
    #[error("corrupt graph value in {column}: {value}")]
    Corrupt {
        /// Column name.
        column: &'static str,
        /// Offending value.
        value: String,
    },
}
