//! The exploration graph, persisted in SQLite.
//!
//! The graph is the shared memory of every role. Its nouns:
//!
//! - [`Node`] — a state in the problem space. Three [`NodeKind`]s: the one
//!   **origin** (the commit exploration starts from), **steps** (proposed
//!   units of work, each realised on its own git branch), and **goals**
//!   (what a human wants; never executed, only evaluated against).
//! - [`Edge`] — `source → target`: the target builds on the source. Edges
//!   form a DAG; paths may merge (`A → C`, `B → C`), in which case the
//!   target's branch starts from a merge of its predecessors. An edge into
//!   a goal marks which goal a step is heading for.
//! - [`Plan`] — the planner's latest view: [`PlannedPath`]s from the origin
//!   toward goals. Priorities live on the nodes themselves.
//! - [`Evaluation`] — the evaluator's verdict on one completed step against
//!   one goal. Satisfied evaluations are the **candidates** a human selects
//!   from.
//! - [`AgentRunRecord`] — the journal of agent invocations, linking each
//!   run to its role, node, and raw log.
//!
//! [`Graph`] owns one SQLite connection. Open one per thread; SQLite (WAL,
//! busy timeout) serialises writers.

mod agent_run;
mod edge;
mod error;
mod evaluation;
// Defining module named for the concept it defines — the path echo is deliberate.
mod graph;
mod node;
mod plan;
mod schema;

pub use agent_run::{AgentRunId, AgentRunRecord, AgentRunStatus};
pub use edge::Edge;
pub use error::GraphError;
pub use evaluation::{Evaluation, NewEvaluation};
pub use graph::Graph;
pub use node::{Node, NodeFilter, NodeId, NodeKind, NodeStatus};
pub use plan::{Plan, PlanId, PlannedPath};
