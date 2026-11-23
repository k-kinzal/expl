//! Workspaces: where an agent runs — private clones expl makes for it —
//! and the git worktrees expl keeps for itself.
//!
//! A node *is* a branch (`expl/node-<id>`, or `expl/node-<id>-<n>` when a
//! ref already takes that name: branches are only ever created, never
//! replaced or deleted, so an earlier attempt, an unrelated branch, or the
//! trunk keeps its name). Each verification attempt gets its own [`Workspace`]:
//! a new branch checked out at a unique path under `.expl/worktrees/`, started
//! from the node's predecessors (the first one's tip,
//! merged with the others in order when paths join). A conflicting merge is
//! left in the worktree to be resolved; [`Workspace::resume`] then concludes
//! it and merges the remaining predecessors, so the branch contains every
//! predecessor before the step's own work begins.
//!
//! The worktree is expl's alone. The agent works in an [`AgentClone`]
//! ([`Workspaces::clone_for`]): a repository of its own with the project's
//! whole history, where it may use git as it likes — commit, branch, reset —
//! without reaching the project's refs, the trunk among them. When it is
//! done, expl reads the clone's files with the project's own git and takes
//! them in ([`Workspace::take_in`]), leaving the paths the declaration
//! freezes as they were, and commits the result as the branch tip; a
//! conflict resolution is taken in file by file
//! ([`Workspace::take_resolution`]).
//!
//! Roles that only read (exploration, planning, evaluation) get a
//! [`ScratchWorkspace`]: a detached clone at a node's commit, private to
//! one reaction and removed afterwards. Concurrent reactions therefore never
//! share a working tree. A scratch clone must come back with its files
//! unchanged: [`ScratchWorkspace::changes`] reports changed, deleted, and
//! new files ([`ScratchChanges`]).
//!
//! Processes run in parallel, so git operations that touch shared
//! repository state (worktree registration, branches, commits) are
//! serialised across processes by a lock file.

mod clone;
mod error;
// Defining module named for the concept it defines — the path echo is deliberate.
mod workspace;

pub use clone::AgentClone;
pub use error::WorkspaceError;
pub use workspace::{ScratchChanges, ScratchWorkspace, Workspace, Workspaces};
