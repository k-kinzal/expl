//! Roles: what an agent does when an event reaches it.
//!
//! The vision names four agents — exploration, planning, execution,
//! evaluation. Here a [`Role`] is that responsibility as a reaction
//! contract: the [`EventKind`]s it reacts to, what it reads, which worktree
//! its agent works in, the JSON Schema of the agent's response, and how the
//! response changes the graph (and so which events it causes):
//!
//! | Role | Reacts to | Works in | Changes the graph by (events) |
//! | --- | --- | --- | --- |
//! | exploration | `goal.added`, `node.evaluated`, `node.failed`, `origin.advanced`, `node.revisited` | scratch at the starting node | proposing steps (`node.proposed`) |
//! | planning | `node.proposed`, `node.evaluated`, `node.failed`, `goal.retired`, `origin.advanced` | scratch at the origin | recording a plan, joins, priorities, prunes (`plan.recorded`, `node.planned`, `node.ready`, `node.pruned`) |
//! | execution | `node.ready` | the step's own worktree | committing the step (`node.started`, `node.completed` / `node.failed`, successors' `node.ready`) |
//! | evaluation | `node.completed`, `origin.advanced` | scratch at the step (or origin) | recording verdicts (`node.evaluated`) |
//!
//! `origin.advanced` means the trunk moved: a human merged into the
//! default branch, and a process following it recorded the new origin. Exploration starts again from the new origin,
//! evaluation judges where the trunk now stands against every goal, and
//! planning decides which steps that have not started move onto it.
//!
//! Exploration has a [`Direction`]: forward from a realised node toward the
//! goals, backward from a goal — **counter-exploration**, done when the
//! goal asks for it — past a failed step (new hypotheses in its place),
//! ahead of a step not carried out yet (assuming it succeeds), or,
//! for an agent declared with `scope = "graph"`, across the whole graph.
//!
//! The scratch worktrees of exploration, planning, and evaluation are
//! read-only: an agent that moves HEAD, changes files, or leaves untracked
//! files there fails its run and the reaction with
//! [`RoleError::ScratchModified`], and its response is not acted on.
//!
//! Roles never print and never decide *when* they run: a
//! [`Process`](crate::process::Process) hands them one event at a time.
//!
//! An agent knows only its work: a role's prompt carries the task and the
//! information it needs, never expl or other agents; only planning and a
//! graph-wide exploration see the whole graph.
//! Operator `instructions` are the only addition.

mod context;
mod digest;
mod error;
mod evaluation;
mod execution;
mod exploration;
mod planning;
mod reaction;

pub use context::Context;
pub use error::RoleError;
pub use evaluation::{EvaluationReport, Verdict};
pub use execution::ExecutionReport;
pub use exploration::{ExplorationReport, Rejection};
pub use planning::PlanningReport;
pub use reaction::Reaction;

use std::fmt;
use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::event::{Delivery, EventKind};
use crate::graph::{Graph, GraphError, NodeId};

/// One of the four responsibilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Identifies nodes that could lead toward goals.
    Exploration,
    /// Defines paths of node combinations and sets priorities.
    Planning,
    /// Realises a step on its branch.
    Execution,
    /// Judges completed steps against goals.
    Evaluation,
}

impl Role {
    /// All roles.
    pub const ALL: [Role; 4] = [
        Role::Exploration,
        Role::Planning,
        Role::Execution,
        Role::Evaluation,
    ];

    /// Canonical name, as in `role = "..."`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exploration => "exploration",
            Self::Planning => "planning",
            Self::Execution => "execution",
            Self::Evaluation => "evaluation",
        }
    }

    /// The events this role reacts to.
    #[must_use]
    pub fn reactions(self) -> &'static [EventKind] {
        match self {
            Self::Exploration => &[
                EventKind::GoalAdded,
                EventKind::NodeEvaluated,
                EventKind::NodeFailed,
                EventKind::OriginAdvanced,
                EventKind::NodeRevisited,
            ],
            Self::Planning => &[
                EventKind::NodeProposed,
                EventKind::NodeEvaluated,
                EventKind::NodeFailed,
                EventKind::GoalRetired,
                EventKind::OriginAdvanced,
            ],
            Self::Execution => &[EventKind::NodeReady],
            Self::Evaluation => &[EventKind::NodeCompleted, EventKind::OriginAdvanced],
        }
    }

    /// Whether at most one process of an agent with this role reacts at a
    /// time. Planning considers the whole graph, so two concurrent plans
    /// would only overwrite each other.
    #[must_use]
    pub fn is_exclusive(self) -> bool {
        matches!(self, Self::Planning)
    }

    /// How long an idle process of `agent` waits after its last reaction
    /// before revisiting a random [starting point](Graph::starting_points)
    /// (`node.revisited`), if it revisits at all. Exploration does when
    /// `revisit_cooldown` is declared: one exploration never exhausts a
    /// node, so every starting point keeps growing new steps for as long as
    /// the process runs. Exploration is never held back by how many steps
    /// wait: crowding is planning's to sort out.
    #[must_use]
    pub fn revisit_cooldown(self, agent: &Agent) -> Option<std::time::Duration> {
        match self {
            Self::Exploration => agent
                .declaration()
                .revisit_cooldown
                .map(std::time::Duration::from_secs),
            Self::Planning | Self::Execution | Self::Evaluation => None,
        }
    }

    /// Whether one reaction covers every event of this role recorded
    /// before it started — or, when its [`Report::covered`] says so, before
    /// the graph it was shown. Planning reads the whole graph, so the events
    /// that piled up while it was busy need no reaction of their own.
    #[must_use]
    pub fn coalesces(self) -> bool {
        matches!(self, Self::Planning)
    }

    /// React to the reaction's event.
    ///
    /// # Errors
    ///
    /// [`RoleError`] when the reaction failed; the event is delivered
    /// again.
    pub fn react(self, reaction: &Reaction<'_>, graph: &mut Graph) -> Result<Report, RoleError> {
        match self {
            Self::Exploration => exploration::react(reaction, graph),
            Self::Planning => planning::react(reaction, graph),
            Self::Execution => execution::react(reaction, graph),
            Self::Evaluation => evaluation::react(reaction, graph),
        }
    }

    /// Clean up after the reaction to `delivery` ended for good without
    /// succeeding (its attempts were used up, or the process gave up on
    /// it), so no work is left hanging. Execution fails a step still
    /// held by the delivery (records `node.failed` with `why`), so planning can
    /// replan or prune it; other roles leave nothing behind. Returns the
    /// step failed, if any.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on graph failure.
    pub fn abandon(
        self,
        delivery: &Delivery,
        graph: &mut Graph,
        why: &str,
    ) -> Result<Option<NodeId>, GraphError> {
        match self {
            Self::Execution => execution::abandon(delivery, graph, why),
            Self::Exploration | Self::Planning | Self::Evaluation => Ok(None),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which way an exploration searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// From a realised node toward the goals.
    Forward,
    /// From a goal back toward the origin (counter-exploration).
    Backward,
    /// From a failed step: new hypotheses in its place.
    Failure,
    /// Past a step not carried out yet, assuming it succeeds.
    Ahead,
    /// Across the whole graph at once, from no particular node.
    Survey,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Forward => "forward",
            Self::Backward => "backward",
            Self::Failure => "past the failure",
            Self::Ahead => "ahead",
            Self::Survey => "across the graph",
        })
    }
}

/// What one reaction did.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Report {
    /// Explorations (forward, and backward for counter-exploration goals).
    Exploration {
        /// One per direction explored.
        explorations: Vec<ExplorationReport>,
    },
    /// A plan.
    Planning(PlanningReport),
    /// An execution.
    Execution(ExecutionReport),
    /// An evaluation.
    Evaluation(EvaluationReport),
    /// The event needed no reaction.
    Ignored {
        /// Why.
        reason: String,
    },
}

impl Report {
    /// The newest event this reaction took into account, when it says so:
    /// a coalescing role covers its events up to here and no further.
    #[must_use]
    pub fn covered(&self) -> Option<crate::event::EventId> {
        match self {
            Self::Planning(p) => p.covered,
            Self::Exploration { .. }
            | Self::Execution(_)
            | Self::Evaluation(_)
            | Self::Ignored { .. } => None,
        }
    }

    /// One line for logs and delivery notes.
    #[must_use]
    pub fn summary(&self) -> String {
        let ids = |ids: &[NodeId]| {
            if ids.is_empty() {
                "-".to_owned()
            } else {
                ids.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        };
        match self {
            Self::Exploration { explorations } => explorations
                .iter()
                .map(|e| {
                    let mut line = format!(
                        "explored {} from {}: proposed {}",
                        e.direction,
                        e.from,
                        ids(&e.proposed)
                    );
                    if !e.rejected.is_empty() {
                        let _ = write!(line, " ({} rejected)", e.rejected.len());
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("; "),
            Self::Planning(p) => format!(
                "plan {}: {} path(s), planned {}, joined {}, rebased {}, pruned {}",
                p.plan.0,
                p.paths,
                ids(&p.planned),
                ids(&p.joined),
                ids(&p.rebased),
                ids(&p.pruned)
            ),
            Self::Execution(e) => match e.answer {
                Some(answer) => format!(
                    "{} answered {answer} — {}",
                    e.node,
                    digest::excerpt_to(&e.summary, 160)
                ),
                None if e.branch.is_empty() => format!(
                    "{} {} — {}",
                    e.node,
                    e.status,
                    digest::excerpt_to(&e.summary, 160)
                ),
                None => format!(
                    "{} {} on {} — {}",
                    e.node,
                    e.status,
                    e.branch,
                    digest::excerpt_to(&e.summary, 160)
                ),
            },
            Self::Evaluation(e) if e.verdicts.is_empty() => {
                format!("{}: no goals to evaluate against", e.node)
            }
            Self::Evaluation(e) => e
                .verdicts
                .iter()
                .map(|v| {
                    format!(
                        "{} vs {}: {}/100{}",
                        e.node,
                        v.goal,
                        v.score,
                        if v.satisfied { " satisfied" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
            Self::Ignored { reason } => format!("ignored: {reason}"),
        }
    }
}

/// Operator guidance from `instructions`, if any.
fn instructions_section(agent: &Agent) -> String {
    match &agent.declaration().instructions {
        Some(text) if !text.trim().is_empty() => {
            format!("\n## Instructions\n\n{}\n", text.trim())
        }
        Some(_) | None => String::new(),
    }
}

/// Closing of every role prompt.
fn response_section(schema: &serde_json::Value) -> String {
    format!(
        "\n## Response\n\nFinish with a single JSON object (no prose around it) that matches \
         this JSON Schema:\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(schema).unwrap_or_default()
    )
}
