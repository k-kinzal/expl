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
    /// The attempt already has a worktree path or registration.
    #[error("the attempt already has a worktree at {}", .0.display())]
    AttemptExists(std::path::PathBuf),
    /// Merge conflicts remain after the executor ran.
    #[error("unresolved merge conflicts in: {}", .0.join(", "))]
    UnresolvedConflicts(Vec<String>),
    /// The merge of a predecessor was aborted or concluded by someone
    /// other than expl while its conflicts were being resolved.
    #[error("the merge of predecessor commit {0} is no longer in progress")]
    MergeAbandoned(String),
    /// Every name tried for the step's branch was taken by an existing
    /// ref or reserved for the trunk; none was created, and no existing ref
    /// was touched.
    #[error("no free branch for step {node}: tried {tried} names, from `{first}`")]
    NoFreeBranch {
        /// The step that needs a branch.
        node: NodeId,
        /// The first name tried.
        first: String,
        /// How many names were tried.
        tried: u32,
    },
    /// The step's worktree no longer has the step's own branch checked
    /// out: someone switched it to another branch (the trunk, say) or
    /// detached its HEAD. Nothing is written there, since a write would
    /// land on whatever HEAD points at now.
    #[error(
        "step {node}'s worktree left its branch `{branch}`: HEAD {}; nothing was written there",
        describe_head(.head.as_deref())
    )]
    LeftBranch {
        /// The step whose worktree it is.
        node: NodeId,
        /// The step's own branch, which HEAD should be attached to.
        branch: String,
        /// The ref HEAD is attached to instead (such as `refs/heads/main`),
        /// or `None` when HEAD is detached.
        head: Option<String>,
    },
    /// A scratch worktree, which stays detached, has a branch checked out:
    /// resetting it would move that branch, so nothing is written there.
    #[error(
        "the scratch worktree has `{head}` checked out instead of a detached HEAD; nothing was written there"
    )]
    ScratchAttached {
        /// The ref HEAD is attached to (such as `refs/heads/main`).
        head: String,
    },
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

/// How [`WorkspaceError::LeftBranch`] names where HEAD points.
fn describe_head(head: Option<&str>) -> String {
    match head {
        Some(head) => format!("points at `{head}`"),
        None => "is detached".to_owned(),
    }
}
