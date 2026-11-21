//! [`Lineage`] — the trunk's history read out of the graph — and
//! [`NewOrigin`], one more position for it.
//!
//! The trunk is the default branch the owner moves forward. Every position
//! it held is an origin node; the newest is `active`, the earlier ones are
//! `superseded`. An origin builds on (has edges from) the previous origin
//! and the steps adopted into it, so the lineage is ordinary graph
//! structure and needs no table of its own.
//!
//! An [`Epoch`] is one origin with what it brought: the steps adopted into
//! it directly and every step whose work entered the trunk with it
//! (`absorbed`: the adopted steps and the steps they built on). Each node
//! also belongs to the epoch it was explored from — the newest origin it
//! builds on — which tells how far behind the trunk its base is.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::graph::{Edge, Node, NodeId, NodeKind, NodeStatus};

/// A new trunk position, about to be recorded by
/// [`Graph::advance_origin`](crate::graph::Graph::advance_origin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrigin {
    /// The active origin this position was observed from: what `adopted`
    /// was computed against. Recording fails if it is no longer active.
    pub previous: NodeId,
    /// Short name (typically the commit subject).
    pub title: String,
    /// Why the trunk moved, in the owner's words.
    pub note: String,
    /// The trunk branch the commit was read from.
    pub branch: Option<String>,
    /// The trunk's commit.
    pub commit: String,
    /// Completed steps whose work the commit takes in.
    pub adopted: Vec<NodeId>,
}

/// One origin and what it brought into the trunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Epoch {
    /// The origin.
    pub origin: NodeId,
    /// The origin it superseded (`None` for the first).
    pub previous: Option<NodeId>,
    /// Steps adopted directly (the origin's step predecessors).
    pub adopted: Vec<NodeId>,
    /// Every step whose work entered the trunk with this origin: the
    /// adopted steps and the steps they built on that were not in the
    /// trunk yet.
    pub absorbed: Vec<NodeId>,
}

/// The trunk's history. See the [module docs](self).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Lineage {
    /// Oldest first.
    pub epochs: Vec<Epoch>,
    /// The active origin.
    pub current: Option<NodeId>,
    /// Node → the origin it was explored from (origins map to themselves;
    /// goals and unconnected nodes are absent).
    #[serde(skip)]
    explored_from: BTreeMap<NodeId, NodeId>,
    /// Step → the origin that absorbed it.
    #[serde(skip)]
    absorbed_by: BTreeMap<NodeId, NodeId>,
}

impl Lineage {
    /// Read the lineage of `nodes` joined by `edges`.
    #[must_use]
    pub fn of(nodes: &[Node], edges: &[Edge]) -> Self {
        let kinds: BTreeMap<NodeId, NodeKind> = nodes.iter().map(|n| (n.id, n.kind)).collect();
        let mut preds: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for edge in edges {
            if kinds.contains_key(&edge.source) && kinds.contains_key(&edge.target) {
                preds.entry(edge.target).or_default().push(edge.source);
            }
        }
        let mut ancestry = Ancestry {
            preds: &preds,
            memo: BTreeMap::new(),
        };

        let origins: Vec<&Node> = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Origin)
            .collect();
        let rank: BTreeMap<NodeId, usize> = origins
            .iter()
            .enumerate()
            .map(|(index, n)| (n.id, index))
            .collect();
        let is_step = |id: &NodeId| kinds.get(id) == Some(&NodeKind::Step);

        let mut lineage = Self {
            current: origins
                .iter()
                .find(|n| n.status == NodeStatus::Active)
                .or(origins.last())
                .map(|n| n.id),
            ..Self::default()
        };
        for origin in &origins {
            let direct = preds.get(&origin.id).map_or(&[][..], Vec::as_slice);
            let previous = direct
                .iter()
                .filter(|p| rank.contains_key(p))
                .max_by_key(|p| rank[p])
                .copied();
            let before = previous.map(|p| ancestry.of(p)).unwrap_or_default();
            let absorbed: Vec<NodeId> = ancestry
                .of(origin.id)
                .iter()
                .filter(|id| is_step(id) && !before.contains(id))
                .copied()
                .collect();
            for step in &absorbed {
                lineage.absorbed_by.insert(*step, origin.id);
            }
            lineage.epochs.push(Epoch {
                origin: origin.id,
                previous,
                adopted: direct.iter().filter(|p| is_step(p)).copied().collect(),
                absorbed,
            });
        }

        for node in nodes {
            let from = match node.kind {
                NodeKind::Origin => Some(node.id),
                NodeKind::Goal => None,
                NodeKind::Step => ancestry
                    .of(node.id)
                    .iter()
                    .filter(|id| rank.contains_key(id))
                    .max_by_key(|id| rank[id])
                    .copied(),
            };
            if let Some(from) = from {
                lineage.explored_from.insert(node.id, from);
            }
        }
        lineage
    }

    /// The epoch of `origin`.
    #[must_use]
    pub fn epoch(&self, origin: NodeId) -> Option<&Epoch> {
        self.epochs.iter().find(|e| e.origin == origin)
    }

    /// The origin `node` was explored from: the newest origin it builds on
    /// (an origin is its own).
    #[must_use]
    pub fn explored_from(&self, node: NodeId) -> Option<NodeId> {
        self.explored_from.get(&node).copied()
    }

    /// The origin that took `step`'s work into the trunk, if any.
    #[must_use]
    pub fn absorbed_by(&self, step: NodeId) -> Option<NodeId> {
        self.absorbed_by.get(&step).copied()
    }

    /// Whether `step`'s work is in the trunk.
    #[must_use]
    pub fn in_trunk(&self, step: NodeId) -> bool {
        self.absorbed_by.contains_key(&step)
    }

    /// How many times the trunk has advanced since `origin` (0 for the
    /// current origin).
    #[must_use]
    pub fn behind(&self, origin: NodeId) -> Option<usize> {
        let index = self.epochs.iter().position(|e| e.origin == origin)?;
        let current = self
            .current
            .and_then(|c| self.epochs.iter().position(|e| e.origin == c))?;
        Some(current.saturating_sub(index))
    }
}

/// Memoised transitive predecessors.
struct Ancestry<'a> {
    preds: &'a BTreeMap<NodeId, Vec<NodeId>>,
    memo: BTreeMap<NodeId, BTreeSet<NodeId>>,
}

impl Ancestry<'_> {
    fn of(&mut self, id: NodeId) -> BTreeSet<NodeId> {
        if let Some(found) = self.memo.get(&id) {
            return found.clone();
        }
        // Iterative walk: lineages can be long, and edges form a DAG.
        let mut seen = BTreeSet::new();
        let mut stack: Vec<NodeId> = self.preds.get(&id).cloned().unwrap_or_default();
        while let Some(next) = stack.pop() {
            if next == id || !seen.insert(next) {
                continue;
            }
            if let Some(known) = self.memo.get(&next) {
                seen.extend(known.iter().copied());
            } else if let Some(more) = self.preds.get(&next) {
                stack.extend(more.iter().copied());
            }
        }
        self.memo.insert(id, seen.clone());
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64, kind: NodeKind, status: NodeStatus) -> Node {
        Node {
            id: NodeId(id),
            kind,
            status,
            title: format!("n{id}"),
            objective: String::new(),
            priority: 0,
            counter_exploration: false,
            branch: None,
            head_commit: None,
            outcome: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn edge(source: i64, target: i64) -> Edge {
        Edge {
            source: NodeId(source),
            target: NodeId(target),
        }
    }

    #[test]
    fn epochs_record_what_each_origin_took_in() {
        use NodeKind::{Goal, Origin, Step};
        use NodeStatus::{Active, Completed, Planned, Superseded};
        // 1 origin → 3 → 4, 1 → 5; goal 2. Origin 6 adopts 4 (and so 3).
        // 7 explored from 6; 5 is left behind on 1.
        let nodes = [
            node(1, Origin, Superseded),
            node(2, Goal, Active),
            node(3, Step, Completed),
            node(4, Step, Completed),
            node(5, Step, Completed),
            node(6, Origin, Active),
            node(7, Step, Planned),
        ];
        let edges = [
            edge(1, 3),
            edge(3, 4),
            edge(1, 5),
            edge(4, 2),
            edge(1, 6),
            edge(4, 6),
            edge(6, 7),
        ];
        let lineage = Lineage::of(&nodes, &edges);
        assert_eq!(lineage.current, Some(NodeId(6)));
        assert_eq!(lineage.epochs.len(), 2);
        let second = lineage.epoch(NodeId(6)).expect("epoch");
        assert_eq!(second.previous, Some(NodeId(1)));
        assert_eq!(second.adopted, vec![NodeId(4)]);
        assert_eq!(second.absorbed, vec![NodeId(3), NodeId(4)]);
        assert!(lineage.epoch(NodeId(1)).expect("first").absorbed.is_empty());

        assert_eq!(lineage.absorbed_by(NodeId(3)), Some(NodeId(6)));
        assert!(!lineage.in_trunk(NodeId(5)));
        assert_eq!(lineage.explored_from(NodeId(5)), Some(NodeId(1)));
        assert_eq!(lineage.explored_from(NodeId(7)), Some(NodeId(6)));
        assert_eq!(lineage.explored_from(NodeId(2)), None);
        assert_eq!(lineage.behind(NodeId(1)), Some(1));
        assert_eq!(lineage.behind(NodeId(6)), Some(0));
    }

    #[test]
    fn a_single_origin_is_its_own_epoch() {
        let nodes = [node(1, NodeKind::Origin, NodeStatus::Active)];
        let lineage = Lineage::of(&nodes, &[]);
        assert_eq!(lineage.current, Some(NodeId(1)));
        assert_eq!(lineage.epochs[0].previous, None);
        assert_eq!(lineage.behind(NodeId(1)), Some(0));
    }
}
