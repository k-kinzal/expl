//! [`GoalTree`] — which goals are subgoals of which.
//!
//! A subgoal is a goal with edges to the goals it serves (`subgoal →
//! goal`): a hypothesis about a stage or precondition on the way to them.
//! Subgoals nest, and one subgoal may serve several goals. Serving is not
//! requiring: several subgoals of one goal are alternative routes, so the
//! tree orders work rather than gating it.

use std::collections::{BTreeMap, BTreeSet};

use crate::graph::{Edge, Node, NodeId, NodeKind};

/// The subgoal relation of a graph. See the [module docs](self).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalTree {
    /// Goal → the goals it is a subgoal of.
    parents: BTreeMap<NodeId, Vec<NodeId>>,
}

impl GoalTree {
    /// Read the subgoal relation of `nodes` joined by `edges`.
    #[must_use]
    pub fn of(nodes: &[Node], edges: &[Edge]) -> Self {
        let goals: BTreeSet<NodeId> = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Goal)
            .map(|n| n.id)
            .collect();
        let mut parents: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for edge in edges {
            if goals.contains(&edge.source) && goals.contains(&edge.target) {
                parents.entry(edge.source).or_default().push(edge.target);
            }
        }
        Self { parents }
    }

    /// Whether any goal is a subgoal.
    #[must_use]
    pub fn is_flat(&self) -> bool {
        self.parents.is_empty()
    }

    /// The goals `goal` is a subgoal of (empty for a top-level goal).
    #[must_use]
    pub fn parents(&self, goal: NodeId) -> &[NodeId] {
        self.parents.get(&goal).map_or(&[], Vec::as_slice)
    }

    /// The goals `goal` serves, directly or through other subgoals,
    /// nearest first.
    #[must_use]
    pub fn ancestors(&self, goal: NodeId) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut frontier: Vec<NodeId> = self.parents(goal).to_vec();
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for id in frontier {
                if id != goal && !found.contains(&id) {
                    found.push(id);
                    next.extend_from_slice(self.parents(id));
                }
            }
            frontier = next;
        }
        found
    }

    /// The direct subgoals of `goal`, by id.
    #[must_use]
    pub fn children(&self, goal: NodeId) -> Vec<NodeId> {
        self.parents
            .iter()
            .filter(|(_, parents)| parents.contains(&goal))
            .map(|(child, _)| *child)
            .collect()
    }

    /// How deep `goal` sits: 0 for a top-level goal, else one more than
    /// its deepest parent.
    #[must_use]
    pub fn depth(&self, goal: NodeId) -> usize {
        fn walk(tree: &GoalTree, goal: NodeId, seen: &mut Vec<NodeId>) -> usize {
            if seen.contains(&goal) {
                return 0;
            }
            seen.push(goal);
            let depth = tree
                .parents(goal)
                .iter()
                .map(|p| 1 + walk(tree, *p, seen))
                .max()
                .unwrap_or(0);
            seen.pop();
            depth
        }
        walk(self, goal, &mut Vec::new())
    }

    /// The goals of `among` that serve no other goal of `among`.
    #[must_use]
    pub fn roots(&self, among: &[NodeId]) -> Vec<NodeId> {
        among
            .iter()
            .filter(|id| !self.ancestors(**id).iter().any(|a| among.contains(a)))
            .copied()
            .collect()
    }

    /// The goals of `among` that no other goal of `among` serves: the most
    /// specific aims in the set.
    #[must_use]
    pub fn leaves(&self, among: &[NodeId]) -> Vec<NodeId> {
        among
            .iter()
            .filter(|id| {
                !among
                    .iter()
                    .any(|other| other != *id && self.ancestors(*other).contains(id))
            })
            .copied()
            .collect()
    }

    /// `goals` in tree order: each top-level goal (of `goals`) followed by
    /// its subgoals, depth first; a subgoal of several goals comes once,
    /// under the first.
    #[must_use]
    pub fn ordered<'a>(&self, goals: &[&'a Node]) -> Vec<&'a Node> {
        let ids: Vec<NodeId> = goals.iter().map(|g| g.id).collect();
        let by_id: BTreeMap<NodeId, &'a Node> = goals.iter().map(|g| (g.id, *g)).collect();
        let mut out: Vec<&'a Node> = Vec::new();
        let mut stack: Vec<NodeId> = self.roots(&ids).into_iter().rev().collect();
        while let Some(id) = stack.pop() {
            if out.iter().any(|g| g.id == id) {
                continue;
            }
            if let Some(goal) = by_id.get(&id) {
                out.push(goal);
            }
            let children: Vec<NodeId> = self
                .children(id)
                .into_iter()
                .filter(|c| ids.contains(c))
                .collect();
            stack.extend(children.into_iter().rev());
        }
        // Goals cut off from every root (a cycle cannot exist, but stay safe).
        for goal in goals {
            if !out.iter().any(|g| g.id == goal.id) {
                out.push(goal);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::NodeStatus;

    fn goal(id: i64) -> Node {
        Node {
            id: NodeId(id),
            kind: NodeKind::Goal,
            status: NodeStatus::Active,
            title: format!("g{id}"),
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
    fn subgoals_nest_under_the_goals_they_serve() {
        // 2 ← 3 ← 5, 2 ← 4; 6 stands alone. 9 is a step toward 3.
        let nodes = [goal(2), goal(3), goal(4), goal(5), goal(6)];
        let edges = [edge(3, 2), edge(4, 2), edge(5, 3), edge(9, 3)];
        let tree = GoalTree::of(&nodes, &edges);
        let ids = |list: &[i64]| list.iter().map(|i| NodeId(*i)).collect::<Vec<_>>();
        assert!(!tree.is_flat());
        assert_eq!(tree.parents(NodeId(9)), &[] as &[NodeId]);
        assert_eq!(tree.ancestors(NodeId(5)), ids(&[3, 2]));
        assert_eq!(tree.children(NodeId(2)), ids(&[3, 4]));
        assert_eq!(tree.depth(NodeId(5)), 2);
        assert_eq!(tree.depth(NodeId(6)), 0);
        let all = ids(&[2, 3, 4, 5, 6]);
        assert_eq!(tree.roots(&all), ids(&[2, 6]));
        assert_eq!(tree.leaves(&all), ids(&[4, 5, 6]));
        assert_eq!(tree.roots(&ids(&[3, 5])), ids(&[3]));
        let refs: Vec<&Node> = nodes.iter().collect();
        let order: Vec<i64> = tree.ordered(&refs).iter().map(|g| g.id.0).collect();
        assert_eq!(order, [2, 3, 5, 4, 6]);
        assert!(GoalTree::of(&nodes, &[]).is_flat());
    }
}
