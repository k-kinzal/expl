//! Workspaces: where an agent runs — git worktrees owned by expl.
//!
//! A node *is* a branch (`expl/node-<id>`). Execution gets the node's own
//! [`Workspace`]: that branch checked out in its own worktree under
//! `.expl/worktrees/`, started from its predecessors (the first one's tip,
//! merged with the others in order when paths join). A conflicting merge is
//! left in the worktree to be resolved; [`Workspace::resume`] then concludes
//! it and merges the remaining predecessors, so the branch contains every
//! predecessor before the step's own work begins. Whatever the agent leaves
//! there is committed as the branch tip.
//!
//! Roles that only read (exploration, planning, evaluation) get a
//! [`ScratchWorkspace`]: a detached worktree at a node's commit, private to
//! one reaction and removed afterwards. Concurrent reactions therefore never
//! share a working tree. A scratch worktree must come back unchanged:
//! [`ScratchWorkspace::changes`] reports a moved HEAD, changed files, and
//! new untracked files ([`ScratchChanges`]).
//!
//! Processes run in parallel, so git operations that touch shared
//! repository state (worktree registration, branches, commits) are
//! serialised across processes by a lock file.

mod error;
// Defining module named for the concept it defines — the path echo is deliberate.
mod workspace;

pub use error::WorkspaceError;
pub use workspace::{ScratchChanges, ScratchWorkspace, Workspace, Workspaces};
