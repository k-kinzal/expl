//! Where each node sits: a layered drawing of the DAG, left to right.
//!
//! - A node's **column** is the length of the longest path reaching it, so
//!   every edge points rightwards. Goals are pinned to one column after the
//!   deepest step: they are where everything heads.
//! - Within a column nodes are ordered by the mean row of their
//!   predecessors (one barycentre sweep), which keeps most edges short and
//!   uncrossed without a full crossing-minimisation pass.
//! - Columns are centred vertically on row `0`, so rows are fractional.
//! - **The trunk is a straight line.** Every origin sits on row `0`; since
//!   each origin builds on the previous one and on what it adopted, origins
//!   march rightwards as the trunk advances, with history to the left.
//!   Up to the current origin, other nodes keep clear of row `0` so the
//!   trunk line between origins is never covered.
//!
//! The layout is pure and deterministic: the same graph always draws the
//! same way, so nodes only move when the graph changes.

use std::collections::{BTreeMap, HashMap};

use expl_core::{Edge, Node, NodeId, NodeKind};

/// A node's place in the drawing: column and (centred) row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Slot {
    /// Column, from the left.
    pub(crate) column: usize,
    /// Row; columns are centred on `0`.
    pub(crate) row: f32,
}

/// The layout of a whole graph.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Layout {
    /// Every node's slot.
    pub(crate) slots: HashMap<NodeId, Slot>,
    /// Number of columns.
    pub(crate) columns: usize,
    /// Rows in the tallest column.
    pub(crate) rows: usize,
}

impl Layout {
    /// Lay out `nodes` connected by `edges`.
    #[must_use]
    pub(crate) fn of(nodes: &[Node], edges: &[Edge]) -> Self {
        let kinds: HashMap<NodeId, NodeKind> = nodes.iter().map(|n| (n.id, n.kind)).collect();
        let mut preds: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for edge in edges {
            if kinds.contains_key(&edge.source) && kinds.contains_key(&edge.target) {
                preds.entry(edge.target).or_default().push(edge.source);
            }
        }

        // Longest-path depth of every non-goal node (memoised DFS; the
        // `visiting` guard makes a malformed cycle harmless).
        let mut depth: HashMap<NodeId, usize> = HashMap::new();
        let mut visiting = Vec::new();
        for node in nodes.iter().filter(|n| n.kind != NodeKind::Goal) {
            depth_of(node.id, &kinds, &preds, &mut depth, &mut visiting);
        }
        let goal_column = depth.values().max().map_or(0, |d| d + 1);

        let mut columns: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
        for node in nodes {
            let column = if node.kind == NodeKind::Goal {
                goal_column
            } else {
                depth[&node.id]
            };
            columns.entry(column).or_default().push(node.id);
        }

        let trunk_end = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Origin)
            .filter_map(|n| depth.get(&n.id))
            .max()
            .copied();

        let mut slots: HashMap<NodeId, Slot> = HashMap::new();
        let mut rows = 0;
        for (&column, ids) in &mut columns {
            ids.sort_by(|a, b| {
                let ka = barycentre(*a, &preds, &slots);
                let kb = barycentre(*b, &preds, &slots);
                ka.total_cmp(&kb).then(a.cmp(b))
            });
            let origin = ids
                .iter()
                .position(|id| kinds.get(id) == Some(&NodeKind::Origin));
            let mut others: Vec<NodeId> = ids.clone();
            if let Some(index) = origin {
                let id = others.remove(index);
                slots.insert(id, Slot { column, row: 0.0 });
            }
            let keep_clear = origin.is_some() || trunk_end.is_some_and(|end| column < end);
            if keep_clear {
                // Half above the trunk line, half below, in barycentre order.
                let above = others.len() / 2;
                for (index, id) in others.iter().enumerate() {
                    let row = if index < above {
                        index_as_f32(index) - index_as_f32(above)
                    } else {
                        index_as_f32(index - above + 1)
                    };
                    slots.insert(*id, Slot { column, row });
                }
                rows = rows.max(2 * (others.len() - above) + 1);
            } else {
                let centre = centre_of(others.len());
                for (index, id) in others.iter().enumerate() {
                    slots.insert(
                        *id,
                        Slot {
                            column,
                            row: index_as_f32(index) - centre,
                        },
                    );
                }
                rows = rows.max(others.len());
            }
        }

        Self {
            columns: columns.keys().next_back().map_or(0, |c| c + 1),
            slots,
            rows,
        }
    }
}

fn depth_of(
    id: NodeId,
    kinds: &HashMap<NodeId, NodeKind>,
    preds: &HashMap<NodeId, Vec<NodeId>>,
    depth: &mut HashMap<NodeId, usize>,
    visiting: &mut Vec<NodeId>,
) -> usize {
    if let Some(&d) = depth.get(&id) {
        return d;
    }
    if visiting.contains(&id) {
        return 0;
    }
    visiting.push(id);
    let mut d = 0;
    for &source in preds.get(&id).map_or(&[][..], Vec::as_slice) {
        if kinds.get(&source) != Some(&NodeKind::Goal) {
            d = d.max(depth_of(source, kinds, preds, depth, visiting) + 1);
        }
    }
    visiting.pop();
    depth.insert(id, d);
    d
}

/// Mean row of the already-placed predecessors of `id`; nodes with none
/// sort to the middle.
fn barycentre(
    id: NodeId,
    preds: &HashMap<NodeId, Vec<NodeId>>,
    slots: &HashMap<NodeId, Slot>,
) -> f32 {
    let rows: Vec<f32> = preds
        .get(&id)
        .into_iter()
        .flatten()
        .filter_map(|p| slots.get(p).map(|s| s.row))
        .collect();
    if rows.is_empty() {
        0.0
    } else {
        rows.iter().sum::<f32>() / index_as_f32(rows.len())
    }
}

fn centre_of(len: usize) -> f32 {
    index_as_f32(len.saturating_sub(1)) / 2.0
}

/// Row indices are small; saturating at `u16::MAX` rows is harmless.
fn index_as_f32(index: usize) -> f32 {
    f32::from(u16::try_from(index).unwrap_or(u16::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use expl_core::NodeStatus;

    fn node(id: i64, kind: NodeKind) -> Node {
        Node {
            id: NodeId(id),
            kind,
            status: NodeStatus::Proposed,
            title: format!("n{id}"),
            objective: String::new(),
            priority: 0,
            counter_exploration: false,
            investigation: false,
            answer: None,
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
    fn columns_follow_longest_path_and_goals_come_last() {
        // 1 → 3 → 4 → 2(goal), 1 → 4 (merge), 1 → 5
        let nodes = [
            node(1, NodeKind::Origin),
            node(2, NodeKind::Goal),
            node(3, NodeKind::Step),
            node(4, NodeKind::Step),
            node(5, NodeKind::Step),
        ];
        let edges = [edge(1, 3), edge(3, 4), edge(1, 4), edge(4, 2), edge(1, 5)];
        let layout = Layout::of(&nodes, &edges);
        let column = |id| layout.slots[&NodeId(id)].column;
        assert_eq!(column(1), 0);
        assert_eq!(column(3), 1);
        assert_eq!(column(5), 1);
        assert_eq!(column(4), 2, "a merge sits after its deepest source");
        assert_eq!(column(2), 3, "goals sit after the deepest step");
        assert_eq!(layout.columns, 4);
        assert_eq!(layout.rows, 2);
    }

    #[test]
    fn columns_are_centred() {
        let nodes = [
            node(1, NodeKind::Origin),
            node(2, NodeKind::Step),
            node(3, NodeKind::Step),
            node(4, NodeKind::Step),
        ];
        let edges = [edge(1, 2), edge(1, 3), edge(1, 4)];
        let layout = Layout::of(&nodes, &edges);
        let rows: Vec<f32> = [2, 3, 4]
            .iter()
            .map(|id| layout.slots[&NodeId(*id)].row)
            .collect();
        assert_eq!(rows, vec![-1.0, 0.0, 1.0]);
        assert!(layout.slots[&NodeId(1)].row.abs() < f32::EPSILON);
    }

    #[test]
    fn children_follow_their_parents_order() {
        // origin → 2, 3; 3 → 4; 2 → 5. Column 2 should list 5 before 4.
        let nodes = [
            node(1, NodeKind::Origin),
            node(2, NodeKind::Step),
            node(3, NodeKind::Step),
            node(4, NodeKind::Step),
            node(5, NodeKind::Step),
        ];
        let edges = [edge(1, 2), edge(1, 3), edge(3, 4), edge(2, 5)];
        let layout = Layout::of(&nodes, &edges);
        assert!(layout.slots[&NodeId(5)].row < layout.slots[&NodeId(4)].row);
    }

    #[test]
    fn the_trunk_is_a_straight_line_kept_clear() {
        // 1 → 3 → 4, 1 → 5; origin 6 adopts 4; 6 → 7, 6 → 8.
        let nodes = [
            node(1, NodeKind::Origin),
            node(2, NodeKind::Goal),
            node(3, NodeKind::Step),
            node(4, NodeKind::Step),
            node(5, NodeKind::Step),
            node(6, NodeKind::Origin),
            node(7, NodeKind::Step),
            node(8, NodeKind::Step),
        ];
        let edges = [
            edge(1, 3),
            edge(3, 4),
            edge(1, 5),
            edge(1, 6),
            edge(4, 6),
            edge(6, 7),
            edge(6, 8),
            edge(7, 2),
        ];
        let layout = Layout::of(&nodes, &edges);
        let slot = |id| layout.slots[&NodeId(id)];
        assert_eq!(slot(6).column, 3, "an origin comes after what it adopted");
        for origin in [1, 6] {
            assert!(slot(origin).row.abs() < f32::EPSILON, "origins on the line");
        }
        for step in [3, 4, 5] {
            assert!(slot(step).row.abs() >= 1.0, "#{step} clear of the trunk");
        }
        // Past the current origin, its children are centred on it.
        assert!((slot(7).row + slot(8).row).abs() < f32::EPSILON);
        assert_eq!(slot(2).column, 5);
    }

    #[test]
    fn empty_graph_has_no_columns() {
        let layout = Layout::of(&[], &[]);
        assert_eq!(layout.columns, 0);
        assert!(layout.slots.is_empty());
    }
}
