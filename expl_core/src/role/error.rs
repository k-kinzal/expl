//! [`RoleError`].

use thiserror::Error;

use crate::agent::AgentError;
use crate::declaration::DeclarationError;
use crate::graph::GraphError;
use crate::workspace::WorkspaceError;

/// A role could not do its work.
#[derive(Debug, Error)]
pub enum RoleError {
    /// Graph failure.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// Worktree failure.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// The agent failed or replied unusably.
    #[error(transparent)]
    Agent(#[from] AgentError),
    /// The declaration is inconsistent.
    #[error(transparent)]
    Declaration(#[from] DeclarationError),
    /// The database failed outside the graph.
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    /// Filesystem failure (logs).
    #[error("role I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The event does not carry what the role needs.
    #[error("{role} cannot react to {event}: {reason}")]
    Malformed {
        /// The role.
        role: &'static str,
        /// The event.
        event: String,
        /// Why.
        reason: String,
    },
    /// The agent's response does not decode into the role's contract.
    #[error("{role} response does not match its schema: {source}")]
    Response {
        /// The role.
        role: &'static str,
        /// Decode error.
        #[source]
        source: serde_json::Error,
    },
}
