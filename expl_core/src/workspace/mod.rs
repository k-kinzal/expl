//! Workspaces: where an agent runs — git worktrees owned by expl.
//!
//! A node *is* a branch (`expl/node-<id>`). Execution gets the node's own
//! [`Workspace`]: that branch checked out in its own worktree under
//! `.expl/worktrees/`, started from its predecessors (the first one's tip,
//! merged with the others when paths join; conflicts are left for the
//! executing agent). Whatever the agent leaves there is committed as the
//! branch tip.
//!
//! Roles that only read (exploration, planning, evaluation) get a
//! [`ScratchWorkspace`]: a detached worktree at a node's commit, private to
//! one reaction and removed afterwards. Concurrent reactions therefore never
//! share a working tree.
//!
//! Processes run in parallel, so git operations that touch shared
//! repository state (worktree registration, branches, commits) are
//! serialised across processes by a lock file.

mod error;
// Defining module named for the concept it defines — the path echo is deliberate.
mod workspace;

pub use error::WorkspaceError;
pub use workspace::{ScratchWorkspace, Workspace, Workspaces};
