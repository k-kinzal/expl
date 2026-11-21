//! [`Plan`] and [`PlannedPath`].

use serde::{Deserialize, Serialize};

use crate::graph::NodeId;

/// Identity of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanId(pub i64);

/// One planning result. Each planning replaces the previous plan's paths;
/// older plans remain as history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Identity.
    pub id: PlanId,
    /// The planner's own summary.
    pub summary: String,
    /// Paths toward goals, in the planner's order.
    pub paths: Vec<PlannedPath>,
    /// Creation time.
    pub created_at: String,
}

/// An ordered combination of nodes the planner expects to lead to a goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPath {
    /// The goal this path heads for.
    pub goal: NodeId,
    /// Nodes in execution order (the goal itself is implied).
    pub nodes: Vec<NodeId>,
    /// Why the planner believes in this path.
    pub rationale: String,
}
