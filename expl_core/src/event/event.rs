//! [`Event`] and its vocabulary.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::graph::NodeId;

/// Position of an event in the project's event log; strictly increasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub i64);

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "e{}", self.0)
    }
}

/// What happened. Named as past-tense facts about the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    /// A goal was added (by a human).
    #[serde(rename = "goal.added")]
    GoalAdded,
    /// A goal was retired.
    #[serde(rename = "goal.retired")]
    GoalRetired,
    /// A step was proposed.
    #[serde(rename = "node.proposed")]
    NodeProposed,
    /// A step was given a priority and may be verified.
    #[serde(rename = "node.planned")]
    NodePlanned,
    /// A planned step's predecessors are all completed: it can run now.
    #[serde(rename = "node.ready")]
    NodeReady,
    /// A step's verification started.
    #[serde(rename = "node.started")]
    NodeStarted,
    /// A step was realised on its branch.
    #[serde(rename = "node.completed")]
    NodeCompleted,
    /// A step's verification failed.
    #[serde(rename = "node.failed")]
    NodeFailed,
    /// A completed step was judged against goals.
    #[serde(rename = "node.evaluated")]
    NodeEvaluated,
    /// A step was discarded.
    #[serde(rename = "node.pruned")]
    NodePruned,
    /// An idle exploration picked a node at random to explore from again:
    /// a realised node (forward) or a counter-exploration goal (backward).
    /// One exploration never exhausts a starting point, so each one keeps
    /// growing new steps.
    #[serde(rename = "node.revisited")]
    NodeRevisited,
    /// A plan (paths toward goals) was recorded.
    #[serde(rename = "plan.recorded")]
    PlanRecorded,
    /// The trunk moved (a human merged into the default branch): a new
    /// origin supersedes the previous one.
    #[serde(rename = "origin.advanced")]
    OriginAdvanced,
}

impl EventKind {
    /// Every kind.
    pub const ALL: &'static [EventKind] = &[
        Self::GoalAdded,
        Self::GoalRetired,
        Self::NodeProposed,
        Self::NodePlanned,
        Self::NodeReady,
        Self::NodeStarted,
        Self::NodeCompleted,
        Self::NodeFailed,
        Self::NodeEvaluated,
        Self::NodePruned,
        Self::NodeRevisited,
        Self::PlanRecorded,
        Self::OriginAdvanced,
    ];

    /// Canonical name (also the stored value).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GoalAdded => "goal.added",
            Self::GoalRetired => "goal.retired",
            Self::NodeProposed => "node.proposed",
            Self::NodePlanned => "node.planned",
            Self::NodeReady => "node.ready",
            Self::NodeStarted => "node.started",
            Self::NodeCompleted => "node.completed",
            Self::NodeFailed => "node.failed",
            Self::NodeEvaluated => "node.evaluated",
            Self::NodePruned => "node.pruned",
            Self::NodeRevisited => "node.revisited",
            Self::PlanRecorded => "plan.recorded",
            Self::OriginAdvanced => "origin.advanced",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| format!("unknown event kind `{s}`"))
    }
}

/// One immutable fact about the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Position in the log.
    pub id: EventId,
    /// What happened.
    pub kind: EventKind,
    /// The node it happened to, if any.
    pub node: Option<NodeId>,
    /// When (UTC, RFC 3339).
    pub created_at: String,
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.node {
            Some(node) => write!(f, "{} {} {node}", self.id, self.kind),
            None => write!(f, "{} {}", self.id, self.kind),
        }
    }
}
