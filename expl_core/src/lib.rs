//! The general-purpose exploration application built on [`expl_runtime`].
//!
//! The graph holds origins, hypotheses and goals. Each mutation produces
//! an event. Configured consumers subscribe to those events and run handlers
//! whose results can change the graph and produce further events. Worker
//! replicas share an inbox, so independent deliveries can run in parallel.
//!
//! - [`graph`] — nodes, edges, plans, evaluations and the events they produce.
//! - [`event`] — subscriptions and leased deliveries.
//! - [`handler`] — graph operations that run in response to an event.
//! - [`agent`] — the runtime drivers that get a structured response from a command.
//! - [`process`] — workers and the process table behind `expl ps`.
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

pub mod declaration;
pub mod doctor;
pub mod event;
pub mod git;
pub mod graph;
pub mod handler;
pub mod overview;
pub mod process;
pub mod project;
pub mod store;
mod telemetry;
pub mod trunk;
pub mod workspace;

pub use expl_runtime::{agent, prompt, testing};

pub use agent::{Agent, AgentDriver, AgentError, AgentRun, DriverKind};
pub use declaration::{
    CliDeclaration, ConsumerDeclaration, Declaration, DeclarationError, RouterDeclaration,
    RoutingPolicy, TelemetryDeclaration, Timeout, TimeoutError,
};
pub use doctor::{Doctor, DoctorError, Inconsistency, Repair};
pub use event::{Consumer, Delivery, Event, EventId, EventKind, Inbox, InboxError, LeaseToken};
pub use graph::{
    AgentRunId, AgentRunRecord, AgentRunStatus, Conclusion, Decision, Edge, Epoch, Evaluation,
    GoalTree, Graph, GraphError, Lineage, NewOrigin, NewStep, Node, NodeFilter, NodeId, NodeKind,
    NodeStatus, Plan, PlannedPath,
};
pub use handler::{Context, Direction, Handler, HandlerError, Reaction, Report};
pub use process::{
    Interrupt, Process, ProcessActivity, ProcessError, ProcessId, ProcessObserver, ProcessOptions,
    ProcessRecord, ProcessState, Processes,
};
pub use project::{Project, ProjectError};
pub use prompt::Prompt;
pub use store::StoreError;
pub use trunk::{Advance, Trunk, TrunkError};
pub use workspace::{
    AgentClone, ScratchChanges, ScratchWorkspace, Workspace, WorkspaceError, Workspaces,
};
