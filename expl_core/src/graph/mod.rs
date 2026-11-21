//! The exploration graph, persisted in SQLite.
//!
//! The graph is the shared memory of every role. Its nouns:
//!
//! - [`Node`] — a state in the problem space. Three [`NodeKind`]s:
//!   **origins** (positions of the trunk — the default branch the owner
//!   moves forward; the newest is where exploration starts), **steps**
//!   (proposed units of work, each realised on its own git branch), and
//!   **goals** (what a human wants; never executed, only evaluated
//!   against).
//! - [`Edge`] — `source → target`: the target builds on the source. Edges
//!   form a DAG; paths may merge (`A → C`, `B → C`), in which case the
//!   target's branch starts from a merge of its predecessors. An edge into
//!   a goal marks which goal a step is heading for. An edge into an origin
//!   records the trunk's lineage: each origin builds on the previous one
//!   and on the steps adopted into it.
//! - [`Lineage`] — the trunk's history read from those edges: one
//!   [`Epoch`] per origin, with what was adopted into it.
//! - [`Plan`] — the planner's latest view: [`PlannedPath`]s from the origin
//!   toward goals. Priorities live on the nodes themselves.
//! - [`Evaluation`] — the evaluator's verdict on one completed step against
//!   one goal. Satisfied evaluations are the **candidates** a human selects
//!   from.
//! - [`AgentRunRecord`] — the journal of agent invocations, linking each
//!   run to its agent, process, event, node, and raw log.
//!
//! Every mutation records its [`Event`](crate::event::Event)s in the same
//! transaction, so the event log and the graph never disagree: agents react
//! to what the graph says happened.
//!
//! [`Graph`] owns one SQLite connection. Open one per thread; SQLite (WAL,
//! busy timeout) serialises writers.

mod agent_run;
mod edge;
mod error;
mod evaluation;
// Defining module named for the concept it defines — the path echo is deliberate.
mod graph;
mod lineage;
mod node;
mod plan;

pub use agent_run::{AgentRunId, AgentRunRecord, AgentRunStatus};
pub use edge::Edge;
pub use error::GraphError;
pub use evaluation::{Evaluation, NewEvaluation};
pub use graph::Graph;
pub use lineage::{Epoch, Lineage, NewOrigin};
pub use node::{Node, NodeFilter, NodeId, NodeKind, NodeStatus};
pub use plan::{Plan, PlanId, PlannedPath};
