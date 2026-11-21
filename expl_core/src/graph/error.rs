//! [`GraphError`].

use thiserror::Error;

use crate::graph::{NodeId, NodeKind};

/// A graph operation failed.
#[derive(Debug, Error)]
pub enum GraphError {
    /// SQLite failure.
    #[error("graph database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database could not be opened.
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    /// No node with this id.
    #[error("node {0} does not exist")]
    NodeNotFound(NodeId),
    /// The node is not in a state that allows the operation.
    #[error("node {node} must be {expected}")]
    NotInStatus {
        /// The node.
        node: NodeId,
        /// What it must be.
        expected: &'static str,
    },
    /// The graph has no origin (not initialised).
    #[error("the graph has no origin node")]
    NoOrigin,
    /// An origin already exists.
    #[error("the graph already has an origin node")]
    OriginExists,
    /// The active origin is no longer the one a new origin was observed
    /// from: another process recorded a move of the trunk first.
    #[error("the active origin is {current}, not {expected}: the trunk was followed meanwhile")]
    OriginMoved {
        /// The origin the observation was made from.
        expected: NodeId,
        /// The active origin now.
        current: NodeId,
    },
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
