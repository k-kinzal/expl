//! Core runtime for expl: an exploration **graph** grown by four **roles**,
//! each carried out by an **agent** inside a **workspace**.
//!
//! The domain is a handful of nouns, each living with the concept it
//! represents:
//!
//! - [`graph`] — the exploration graph persisted in SQLite: [`Node`]s
//!   (origin, step, goal), directed edges between them, the latest
//!   [`Plan`], and [`Evaluation`]s. The graph is the single source of truth;
//!   every role reads it and writes back into it.
//! - [`agent`] — which AI agent CLI runs and how it is invoked. Mirrors
//!   iter: [`Agent`] realises the invocation cycle once, an
//!   [`AgentDriver`] translates for one CLI (Claude Code, Codex, generic).
//! - [`workspace`] — where an agent runs: one git worktree per node, on the
//!   node's own branch. A node *is* a branch; merging paths (`A → C`,
//!   `B → C`) is a git merge of the predecessor branches.
//! - [`role`] — the four responsibilities from the vision: the
//!   [`Explorer`](role::Explorer) proposes nodes (forward toward a goal, or
//!   backward from it as counter-exploration), the
//!   [`Planner`](role::Planner) draws paths and sets priorities, the
//!   [`Executor`](role::Executor) realises a node in its worktree, and the
//!   [`Evaluator`](role::Evaluator) judges completed nodes against goals.
//! - [`runner`] — the [`Runner`] repeats **cycles** (explore → plan →
//!   execute → evaluate). Exploration never finishes by itself; choosing a
//!   goal among the satisfying candidates is the human's responsibility.
//!
//! Around the domain sit the [`project`] (where the declaration and the
//! local state live) and the [`declaration`] (`expl.toml`, what the operator
//! wrote).

#![deny(rust_2018_idioms)]

pub mod agent;
pub mod declaration;
pub mod git;
pub mod graph;
pub mod project;
pub mod prompt;
pub mod role;
pub mod runner;
pub mod workspace;

pub use agent::{Agent, AgentDriver, AgentError, AgentKind, AgentRun};
pub use declaration::{Declaration, DeclarationError};
pub use graph::{
    AgentRunId, AgentRunRecord, AgentRunStatus, Edge, Evaluation, Graph, GraphError, Node,
    NodeFilter, NodeId, NodeKind, NodeStatus, Plan, PlannedPath,
};
pub use project::{Project, ProjectError, ProjectLock};
pub use prompt::Prompt;
pub use role::{Context, Direction, Role, RoleError};
pub use runner::{CycleReport, Runner, RunnerError, RunnerEvent, RunnerObserver};
pub use workspace::{Workspace, WorkspaceError, Workspaces};
