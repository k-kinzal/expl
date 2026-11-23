//! Built-in graph operations for event consumers.
//!
//! A [`Handler`] prepares the workspace and request for a delivered event,
//! validates the response, and applies the result to the graph. This may
//! produce more events. The consumer declares which events to deliver and
//! which command supplies the response.
//!
//! Exploration can start from a node, a goal, a failed or speculative step,
//! or the graph as a whole. Planning reads the graph and coalesces deliveries
//! already covered by a plan. Verification handles ready change steps or
//! investigations. Evaluation reads the results against the active goals.
//!
//! A handler working in a read-only scratch clone rejects changes to HEAD or
//! files with [`HandlerError::ScratchModified`].

mod context;
mod digest;
mod error;
mod evaluation;
mod exploration;
mod planning;
mod reaction;
mod verification;

pub use context::Context;
pub use error::HandlerError;
pub use evaluation::{EvaluationReport, Verdict};
pub use exploration::{ExplorationReport, Rejection};
pub use planning::PlanningReport;
pub use reaction::Reaction;
pub use verification::VerificationReport;

use std::fmt;
use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::event::{Consumer, Delivery, EventKind};
use crate::graph::{Graph, GraphError, NodeId};

/// Built-in operation to run for a delivered event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Handler {
    /// Identifies nodes that could lead toward goals.
    Exploration,
    /// Defines paths of node combinations and sets priorities.
    Planning,
    /// Realises a step on its branch.
    #[serde(alias = "execution")]
    Verification,
    /// Judges completed steps against goals.
    Evaluation,
}

impl Handler {
    /// All built-in handlers.
    pub const ALL: [Handler; 4] = [
        Handler::Exploration,
        Handler::Planning,
        Handler::Verification,
        Handler::Evaluation,
    ];

    /// Canonical name, as in `handler = "..."`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exploration => "exploration",
            Self::Planning => "planning",
            Self::Verification => "verification",
            Self::Evaluation => "evaluation",
        }
    }

    /// Supported events and the default subscriptions for legacy declarations.
    #[must_use]
    pub(crate) fn events(self) -> &'static [EventKind] {
        match self {
            // Exploration grows from a failure once evaluation concluded
            // about it (`node.evaluated`): it may be continued instead, and
            // otherwise comes with why it cannot work. `node.failed` itself
            // is explored when no consumer handles failure evaluation.
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
            Self::Verification => &[EventKind::NodeReady],
            Self::Evaluation => &[
                EventKind::NodeCompleted,
                EventKind::NodeFailed,
                EventKind::OriginAdvanced,
            ],
        }
    }

    /// Whether at most one worker of a consumer with this handler reacts at a
    /// time. Planning considers the whole graph, so two concurrent plans
    /// would only overwrite each other.
    #[must_use]
    pub fn is_exclusive(self) -> bool {
        matches!(self, Self::Planning)
    }

    /// Whether one reaction covers every event of this consumer recorded
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
    /// [`HandlerError`] when the reaction failed; the event is delivered
    /// again.
    pub fn react(self, reaction: &Reaction<'_>, graph: &mut Graph) -> Result<Report, HandlerError> {
        match self {
            Self::Exploration => exploration::react(reaction, graph),
            Self::Planning => planning::react(reaction, graph),
            Self::Verification => verification::react(reaction, graph),
            Self::Evaluation => evaluation::react(reaction, graph),
        }
    }

    /// Clean up after the reaction to `delivery` ended for good without
    /// succeeding (its attempts were used up, or the process gave up on
    /// it), so no work is left hanging. Verification fails a step still
    /// held by the delivery (records `node.failed` with `why`), so planning can
    /// replan or prune it; other handlers leave nothing behind. A step
    /// settled elsewhere already is left as it is. Returns what became of
    /// the delivery's step.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on graph failure.
    pub fn abandon(
        self,
        delivery: &Delivery,
        graph: &mut Graph,
        why: &str,
    ) -> Result<Abandonment, GraphError> {
        match self {
            Self::Verification => verification::abandon(delivery, graph, why),
            Self::Exploration | Self::Planning | Self::Evaluation => Ok(Abandonment::Nothing),
        }
    }
}

/// What [`Handler::abandon`] did with the step of a delivery that ended for
/// good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abandonment {
    /// Nothing was left behind: the handler holds no steps, the event names
    /// none, or the delivery does not hold its step (another one does).
    Nothing,
    /// The step the delivery still held, now failed.
    Failed(NodeId),
    /// The step was settled elsewhere already (completed, failed or
    /// pruned) and is left as it is: the delivery has nothing left to do.
    Settled(NodeId),
}

impl Abandonment {
    /// The step that was failed, if any.
    #[must_use]
    pub fn failed(self) -> Option<NodeId> {
        match self {
            Self::Failed(id) => Some(id),
            Self::Nothing | Self::Settled(_) => None,
        }
    }
}

impl fmt::Display for Handler {
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
    /// A verification.
    Verification(VerificationReport),
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
    /// a coalescing handler covers its events up to here and no further.
    #[must_use]
    pub fn covered(&self) -> Option<crate::event::EventId> {
        match self {
            Self::Planning(p) => p.covered,
            Self::Exploration { .. }
            | Self::Verification(_)
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
            Self::Verification(e) => match e.answer {
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
fn instructions_section(consumer: &Consumer) -> String {
    match &consumer.declaration().instructions {
        Some(text) if !text.trim().is_empty() => {
            format!("\n## Instructions\n\n{}\n", text.trim())
        }
        Some(_) | None => String::new(),
    }
}

/// Closing of every handler prompt.
fn response_section(schema: &serde_json::Value) -> String {
    format!(
        "\n## Response\n\nFinish with a single JSON object (no prose around it) that matches \
         this JSON Schema:\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(schema).unwrap_or_default()
    )
}
