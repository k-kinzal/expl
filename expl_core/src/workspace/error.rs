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
    /// The node has no recorded commit to start from.
    #[error("node {0} has no recorded commit")]
    NoCommit(NodeId),
    /// Merge conflicts remain after the executor ran.
    #[error("unresolved merge conflicts in: {}", .0.join(", "))]
    UnresolvedConflicts(Vec<String>),
    /// The merge of a predecessor was aborted or concluded by someone
    /// other than expl while its conflicts were being resolved.
    #[error("the merge of predecessor commit {0} is no longer in progress")]
    MergeAbandoned(String),
    /// Predecessor commits are still waiting to be merged.
    #[error("predecessor commits not merged yet: {}", .0.join(", "))]
    JoinIncomplete(Vec<String>),
    /// A predecessor commit is not an ancestor of the branch tip.
    #[error("predecessor commit {commit} is not an ancestor of the branch tip {tip}")]
    PredecessorNotMerged {
        /// The predecessor commit that is missing.
        commit: String,
        /// The branch tip that should contain it.
        tip: String,
    },
}
