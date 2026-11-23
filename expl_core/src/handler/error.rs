//! [`HandlerError`].

use thiserror::Error;

use crate::agent::AgentError;
use crate::declaration::DeclarationError;
use crate::graph::GraphError;
use crate::workspace::{ScratchChanges, WorkspaceError};

/// A handler could not do its work.
#[derive(Debug, Error)]
pub enum HandlerError {
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
    #[error("handler I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The event does not carry what the handler needs.
    #[error("{handler} cannot react to {event}: {reason}")]
    Malformed {
        /// The handler.
        handler: &'static str,
        /// The event.
        event: String,
        /// Why.
        reason: String,
    },
    /// A read-only agent changed its scratch worktree: it moved HEAD,
    /// changed files, or left untracked files. Its response is not acted
    /// on.
    #[error("{handler} agent modified its read-only worktree: {changes}")]
    ScratchModified {
        /// The handler.
        handler: &'static str,
        /// What changed, naming the changed paths.
        changes: ScratchChanges,
    },
}
