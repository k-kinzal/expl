//! [`Snapshot`] — the graph as it stood at one moment.

use crate::event::EventId;
use crate::graph::{Conclusion, Edge, Evaluation, Node, NodeId, NodeKind, NodeStatus};

/// Every node, edge, and evaluation as they stood at one moment, with the
/// newest event recorded by then. Read in one transaction, so the nodes and
/// the event log agree: a role that shows its agent a snapshot can act on
/// exactly what the agent was shown, and leave later events for later.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Every node, by id.
    pub nodes: Vec<Node>,
    /// Every edge.
    pub edges: Vec<Edge>,
    /// Every evaluation, oldest first.
    pub evaluations: Vec<Evaluation>,
    /// Every conclusion about a step, oldest first.
    pub conclusions: Vec<Conclusion>,
    /// The newest event at that moment (`None` while there is none).
    pub through: Option<EventId>,
}

impl Snapshot {
    /// The node with `id`, if it existed then.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// The latest conclusion about `id`, if any.
    #[must_use]
    pub fn conclusion(&self, id: NodeId) -> Option<&Conclusion> {
        self.conclusions.iter().rev().find(|c| c.node == id)
    }

    /// Goals active then.
    #[must_use]
    pub fn goals(&self) -> Vec<Node> {
        self.nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Goal && n.status == NodeStatus::Active)
            .cloned()
            .collect()
    }
}
