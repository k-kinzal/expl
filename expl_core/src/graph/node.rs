//! [`Node`] and its vocabulary.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Stable identity of a node: a small integer, so humans and agents can
/// refer to nodes without copying opaque ids (`#12`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub i64);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

impl FromStr for NodeId {
    type Err = std::num::ParseIntError;

    /// Accepts `12` and `#12`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim().trim_start_matches('#').parse().map(Self)
    }
}

/// What a node stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// The starting point: the commit exploration began from. Exactly one.
    Origin,
    /// A unit of work toward some goal, realised on its own branch.
    Step,
    /// What the human wants. Evaluated against; never executed.
    Goal,
}

/// Where a node is in its lifecycle.
///
/// Steps move `proposed → planned → running → completed | failed`, and may
/// be `pruned` by the planner (or a human) before running. The origin is
/// always `completed`. Goals are `active` until a human prunes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Found by exploration, not yet planned.
    Proposed,
    /// Given a priority by the planner; eligible for execution once every
    /// predecessor is completed.
    Planned,
    /// An executor is working on it.
    Running,
    /// Executed; its branch holds the result.
    Completed,
    /// Execution failed.
    Failed,
    /// Discarded; never executed.
    Pruned,
    /// A goal that is being pursued.
    Active,
}

macro_rules! text_enum {
    ($ty:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $ty {
            /// All variants.
            pub const ALL: &'static [$ty] = &[$($ty::$variant),+];

            /// Canonical lowercase name (also the stored value).
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self { $($ty::$variant => $text),+ }
            }
        }

        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $ty {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok($ty::$variant),)+
                    other => Err(format!(
                        "unknown {} `{other}` (expected one of: {})",
                        stringify!($ty),
                        [$($text),+].join(", ")
                    )),
                }
            }
        }
    };
}

text_enum!(NodeKind { Origin => "origin", Step => "step", Goal => "goal" });
text_enum!(NodeStatus {
    Proposed => "proposed",
    Planned => "planned",
    Running => "running",
    Completed => "completed",
    Failed => "failed",
    Pruned => "pruned",
    Active => "active",
});

pub(crate) use text_enum;

/// One node of the exploration graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Identity.
    pub id: NodeId,
    /// Origin, step, or goal.
    pub kind: NodeKind,
    /// Lifecycle position.
    pub status: NodeStatus,
    /// Short name.
    pub title: String,
    /// What the node should achieve (for a goal: the goal statement).
    pub objective: String,
    /// Planner-assigned priority; higher runs first.
    pub priority: i64,
    /// Goals only: also explore backward from this goal.
    pub counter_exploration: bool,
    /// Git branch holding this node's work (origin and steps).
    pub branch: Option<String>,
    /// Commit at the tip of the branch once executed (origin: base commit).
    pub head_commit: Option<String>,
    /// Executor's report, or the reason the node was pruned or failed.
    pub outcome: Option<String>,
    /// Creation time (UTC, RFC 3339).
    pub created_at: String,
    /// Last update time (UTC, RFC 3339).
    pub updated_at: String,
}

impl Node {
    /// Whether this node can be the source of an edge.
    #[must_use]
    pub fn can_precede(&self) -> bool {
        matches!(self.kind, NodeKind::Origin | NodeKind::Step)
    }

    /// Whether this node can be the target of an edge.
    #[must_use]
    pub fn can_follow(&self) -> bool {
        matches!(self.kind, NodeKind::Step | NodeKind::Goal)
    }
}

/// Selection for [`Graph::nodes`](crate::graph::Graph::nodes).
#[derive(Debug, Clone, Default)]
pub struct NodeFilter {
    /// Only these kinds (empty: all).
    pub kinds: Vec<NodeKind>,
    /// Only these statuses (empty: all).
    pub statuses: Vec<NodeStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_accepts_hash_prefix() {
        assert_eq!("#12".parse::<NodeId>().expect("parses"), NodeId(12));
        assert_eq!("7".parse::<NodeId>().expect("parses"), NodeId(7));
        assert_eq!(NodeId(3).to_string(), "#3");
    }

    #[test]
    fn status_round_trips_through_text() {
        for status in NodeStatus::ALL {
            assert_eq!(status.as_str().parse::<NodeStatus>().as_ref(), Ok(status));
        }
    }
}
