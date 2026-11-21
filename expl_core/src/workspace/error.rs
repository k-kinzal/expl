//! [`WorkspaceError`].

use thiserror::Error;

use crate::git::GitError;
use crate::graph::NodeId;

/// Creating or reconciling a node worktree failed.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// git failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Filesystem failure.
    #[error("workspace I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// A predecessor has no commit to build on.
    #[error("predecessor {0} has no commit to build on")]
    PredecessorNotRealized(NodeId),
    /// The node has no recorded commit to restore to.
    #[error("node {0} has no recorded commit")]
    NoCommit(NodeId),
    /// Merge conflicts remain after the executor ran.
    #[error("unresolved merge conflicts in: {}", .0.join(", "))]
    UnresolvedConflicts(Vec<String>),
}
