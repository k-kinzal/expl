//! [`Edge`].

use serde::{Deserialize, Serialize};

use crate::graph::NodeId;

/// `source → target`: the target builds on the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    /// The node built upon.
    pub source: NodeId,
    /// The node that builds on it.
    pub target: NodeId,
}
