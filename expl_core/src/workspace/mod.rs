//! Workspaces: where an agent runs — one git worktree per node.
//!
//! A node *is* a branch (`expl/node-<id>`). Its [`Workspace`] is that
//! branch checked out in its own worktree under `.expl/worktrees/`, so
//! executors run in parallel without seeing each other's half-done work.
//!
//! [`Workspaces`] is the enduring collaborator (it knows the repository and
//! the worktree root); a [`Workspace`] is one node's live working tree:
//!
//! - [`Workspaces::create`] starts a step's branch from its predecessors —
//!   the first predecessor's tip, merged with the others when paths join.
//!   Merge conflicts are left in place for the executor to resolve.
//! - [`Workspace::commit`] records the executor's work as the branch tip.
//! - [`Workspace::restore`] returns the tree to the recorded tip after a
//!   role that should only read (explorer, evaluator) has run in it.

mod error;
// Defining module named for the concept it defines — the path echo is deliberate.
mod workspace;

pub use error::WorkspaceError;
pub use workspace::{Workspace, Workspaces};
