//! [`Evaluation`].

use serde::{Deserialize, Serialize};

use crate::graph::NodeId;

/// The evaluator's verdict on one completed step against one goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    /// Row identity.
    pub id: i64,
    /// The evaluated step.
    pub node: NodeId,
    /// The goal it was judged against.
    pub goal: NodeId,
    /// Whether the step's branch satisfies the goal.
    pub satisfied: bool,
    /// Progress toward the goal, 0–100.
    pub score: i64,
    /// The evaluator's reasoning.
    pub reason: String,
    /// Creation time.
    pub created_at: String,
}

/// An evaluation about to be recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvaluation {
    /// The goal judged against.
    pub goal: NodeId,
    /// Verdict.
    pub satisfied: bool,
    /// Progress, clamped to 0–100 when recorded.
    pub score: i64,
    /// Reasoning.
    pub reason: String,
}
