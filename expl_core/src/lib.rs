//! Core runtime for expl: an event-driven exploration of a problem space.
//!
//! The model in one paragraph: the **graph** (SQLite) holds the exploration
//! — origins, steps, goals, and the edges between them. Every change to
//! the graph is recorded as an **event**. Declared **agents** react to the
//! events of their **role** (exploration, planning, verification, evaluation),
//! and their reactions change the graph, which records new events. Agents
//! run as **processes**, managed like Docker containers: start as many as
//! you like; processes of one agent compete for its events, so independent
//! events are handled in parallel. Each step is realised on its own git
//! branch in its own **workspace** (worktree).
//!
//! - [`graph`] — nodes, edges, plans, evaluations; mutations record events.
//! - [`event`] — [`Event`], [`EventKind`], and each agent's [`Inbox`] of
//!   leased deliveries.
//! - [`agent`] — the declared [`Agent`] and the [`AgentDriver`]s that speak
//!   to AI CLIs (Claude Code, Codex, generic).
//! - [`role`] — what each role does when an event reaches it.
//! - [`process`] — [`Process`] (a running agent) and the [`Processes`]
//!   table behind `expl ps`.
//! - [`workspace`] — node worktrees and scratch worktrees.
//! - [`trunk`] — following the default branch: the origin moves with it.
//! - [`doctor`] — checking the graph against git, and the safe repairs.
//! - [`project`], [`declaration`], [`store`] — where it all lives, what the
//!   operator wrote, and the database file.
//! - [`testing`] — wall-clock bounds for expl's own tests, stretched on a
//!   loaded machine.
//!
//! Exploration never finishes by itself; choosing among the steps that
//! satisfy a goal is the human's responsibility.

#![deny(rust_2018_idioms)]

pub mod agent;
pub mod declaration;
pub mod doctor;
pub mod event;
pub mod git;
pub mod graph;
pub mod process;
pub mod project;
pub mod prompt;
pub mod role;
pub mod store;
pub mod testing;
pub mod trunk;
pub mod workspace;

pub use agent::{Agent, AgentDriver, AgentError, AgentRun, DriverKind};
pub use declaration::{
    AgentDeclaration, CliDeclaration, Declaration, DeclarationError, RouterDeclaration,
    RoutingPolicy, Timeout, TimeoutError,
};
pub use doctor::{Doctor, DoctorError, Inconsistency, Repair};
pub use event::{Delivery, Event, EventId, EventKind, Inbox, InboxError, LeaseToken};
pub use graph::{
    AgentRunId, AgentRunRecord, AgentRunStatus, Conclusion, Decision, Edge, Epoch, Evaluation,
    GoalTree, Graph, GraphError, Lineage, NewOrigin, NewStep, Node, NodeFilter, NodeId, NodeKind,
    NodeStatus, Plan, PlannedPath,
};
pub use process::{
    Interrupt, Process, ProcessActivity, ProcessError, ProcessId, ProcessObserver, ProcessOptions,
    ProcessRecord, ProcessState, Processes,
};
pub use project::{Project, ProjectError};
pub use prompt::Prompt;
pub use role::{Context, Direction, Reaction, Report, Role, RoleError};
pub use store::StoreError;
pub use trunk::{Advance, Trunk, TrunkError};
pub use workspace::{
    AgentClone, ScratchChanges, ScratchWorkspace, Workspace, WorkspaceError, Workspaces,
};
