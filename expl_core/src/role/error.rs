//! [`RoleError`].

use thiserror::Error;

use crate::agent::AgentError;
use crate::declaration::DeclarationError;
use crate::graph::GraphError;
use crate::workspace::{ScratchChanges, WorkspaceError};

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
    /// A read-only agent changed its scratch worktree: it moved HEAD,
    /// changed files, or left untracked files. Its response is not acted
    /// on.
    #[error("{role} agent modified its read-only worktree: {changes}")]
    ScratchModified {
        /// The role.
        role: &'static str,
        /// What changed, naming the changed paths.
        changes: ScratchChanges,
    },
}
