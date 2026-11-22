//! Where each node sits: a layered drawing of the DAG, left to right.
//!
//! - A node's **column** is the length of the longest path reaching it, so
//!   every edge points rightwards. Goals are pinned to one column after the
//!   deepest step: they are where everything heads.
//! - Within a column nodes are ordered by the mean row of their
//!   predecessors (one barycentre sweep), which keeps most edges short and
//!   uncrossed without a full crossing-minimisation pass. Nodes that tie —
//!   typically many steps built on the same node — are ordered by
//!   importance from the middle outwards: running, completed and failed
//!   work, then planned steps by priority, then proposals, and pruned steps
//!   at the edges.
//! - **Tall columns fold.** A column with more than [`FOLD_ROWS`] nodes is
//!   laid out in several lanes side by side, filled row by row so the
//!   vertical order is kept, which turns a long line of siblings into a
//!   roughly square block. Columns further right move over to make room.
//! - Columns are centred vertically on row `0`, so rows are fractional.
//! - **The trunk is a straight line.** Every origin sits on row `0`; since
//!   each origin builds on the previous one and on what it adopted, origins
//!   march rightwards as the trunk advances, with history to the left.
//!   Up to the current origin, other nodes keep clear of row `0` so the
//!   trunk line between origins is never covered.
//!
//! The layout is pure and deterministic: the same graph always draws the
//! same way, so nodes only move when the graph changes.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};

use expl_core::{Edge, Node, NodeId, NodeKind, NodeStatus};

/// Most rows a column takes before it folds into lanes.
pub(crate) const FOLD_ROWS: usize = 16;

/// Horizontal distance between the lanes of a folded column, in columns.
pub(crate) const LANE: f32 = 0.35;

/// A node's place in the drawing: column and (centred) row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Slot {
    /// Column (layer), from the left.
    pub(crate) column: usize,
    /// Horizontal position, in columns from the left edge: the column's
    /// start plus its lane in a folded column.
    pub(crate) x: f32,
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
    /// Horizontal extent: the largest `x` of any slot.
    pub(crate) width: f32,
    /// Rows in the tallest column (after folding).
    pub(crate) rows: usize,
}

impl Layout {
    /// Lay out `nodes` connected by `edges`.
    #[must_use]
    pub(crate) fn of(nodes: &[Node], edges: &[Edge]) -> Self {
        let by_id: HashMap<NodeId, &Node> = nodes.iter().map(|n| (n.id, n)).collect();
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
        let mut x = 0.0_f32;
        let mut previous: Option<usize> = None;
        let mut width = 0.0_f32;
        for (&column, ids) in &mut columns {
            // Empty layers in between (none today) still take their room.
            if let Some(previous) = previous {
                x += index_as_f32(column - previous - 1);
            }
            previous = Some(column);
            let bary: HashMap<NodeId, f32> = ids
                .iter()
                .map(|id| (*id, barycentre(*id, &preds, &slots)))
                .collect();
            ids.sort_by(|a, b| {
                bary[a]
                    .total_cmp(&bary[b])
                    .then_with(|| importance(by_id[a]).cmp(&importance(by_id[b])))
                    .then(a.cmp(b))
            });
            let origin = ids
                .iter()
                .position(|id| kinds.get(id) == Some(&NodeKind::Origin));
            let mut others: Vec<NodeId> = ids.clone();
            if let Some(index) = origin {
                let id = others.remove(index);
                slots.insert(
                    id,
                    Slot {
                        column,
                        x,
                        row: 0.0,
                    },
                );
            }
            let others = centre_out_ties(&others, &bary);
            let lanes = lanes_for(others.len());
            let keep_clear = origin.is_some() || trunk_end.is_some_and(|end| column < end);
            let placed = if keep_clear {
                split(others.len(), lanes)
            } else {
                centred(others.len(), lanes)
            };
            for (id, (lane, row)) in others.iter().zip(placed) {
                let at = x + index_as_f32(lane) * LANE;
                width = width.max(at);
                slots.insert(*id, Slot { column, x: at, row });
            }
            let tallest = if keep_clear {
                let below = others.len() - others.len() / 2;
                2 * below.div_ceil(lanes) + 1
            } else {
                others.len().div_ceil(lanes)
            };
            rows = rows.max(tallest);
            x += index_as_f32(lanes - 1) * LANE + 1.0;
        }

        Self {
            columns: columns.keys().next_back().map_or(0, |c| c + 1),
            width,
            slots,
            rows,
        }
    }
}

/// How prominently a node is placed among nodes that tie on barycentre:
/// smaller sits nearer the middle.
fn importance(node: &Node) -> (u8, Reverse<i64>) {
    let class = match node.status {
        NodeStatus::Running => 0,
        NodeStatus::Completed => 1,
        NodeStatus::Failed => 2,
        NodeStatus::Planned => 3,
        NodeStatus::Proposed => 4,
        NodeStatus::Pruned => 6,
        NodeStatus::Active | NodeStatus::Superseded => 5,
    };
    (class, Reverse(node.priority))
}

/// Reorder every run of nodes with the same barycentre (already sorted by
/// importance) so the most important sits in the run's middle and the
/// rest alternate outwards.
fn centre_out_ties(ids: &[NodeId], bary: &HashMap<NodeId, f32>) -> Vec<NodeId> {
    let mut out = Vec::with_capacity(ids.len());
    let mut start = 0;
    while start < ids.len() {
        let key = bary[&ids[start]];
        let end = ids[start..]
            .iter()
            .position(|id| bary[id].total_cmp(&key).is_ne())
            .map_or(ids.len(), |n| start + n);
        let run = &ids[start..end];
        let (mut left, mut right) = (Vec::new(), Vec::new());
        for (rank, id) in run.iter().enumerate() {
            if rank % 2 == 0 {
                right.push(*id);
            } else {
                left.push(*id);
            }
        }
        out.extend(left.into_iter().rev());
        out.extend(right);
        start = end;
    }
    out
}

/// Lanes for a column of `len` nodes: one up to [`FOLD_ROWS`], else enough
/// for a roughly square block of at least [`FOLD_ROWS`] rows.
fn lanes_for(len: usize) -> usize {
    if len <= FOLD_ROWS {
        return 1;
    }
    let mut rows = FOLD_ROWS;
    // Rows ≈ √(1.5 · len): lanes are narrower than columns, so a block of
    // as many rows as lanes would look tall.
    while rows * rows * 2 < len * 3 {
        rows += 1;
    }
    len.div_ceil(rows)
}

/// `(lane, row)` of `len` nodes in `lanes`, filled row by row and centred
/// on row `0`.
fn centred(len: usize, lanes: usize) -> Vec<(usize, f32)> {
    let centre = centre_of(len.div_ceil(lanes));
    (0..len)
        .map(|i| (i % lanes, index_as_f32(i / lanes) - centre))
        .collect()
}

/// `(lane, row)` of `len` nodes in `lanes`, half above the trunk line and
/// half below, row `0` left free. Rows fill outwards from the line, so a
/// partly filled row is the outermost one.
fn split(len: usize, lanes: usize) -> Vec<(usize, f32)> {
    let above = len / 2;
    let mut placed = Vec::with_capacity(len);
    for i in 0..above {
        // Distance from the line, in the order the nodes come.
        let from_line = above - 1 - i;
        placed.push((
            lanes - 1 - from_line % lanes,
            -index_as_f32(from_line / lanes + 1),
        ));
    }
    for i in 0..len - above {
        placed.push((i % lanes, index_as_f32(i / lanes + 1)));
    }
    placed
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
            derived_from: None,
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
        let mut rows: Vec<f32> = [2, 3, 4]
            .iter()
            .map(|id| layout.slots[&NodeId(*id)].row)
            .collect();
        rows.sort_by(f32::total_cmp);
        assert_eq!(rows, vec![-1.0, 0.0, 1.0]);
        assert!(layout.slots[&NodeId(1)].row.abs() < f32::EPSILON);
    }

    #[test]
    fn children_follow_their_parents_order() {
        // origin → 2, 3; 3 → 4; 2 → 5. Column 2 keeps column 1's order.
        let nodes = [
            node(1, NodeKind::Origin),
            node(2, NodeKind::Step),
            node(3, NodeKind::Step),
            node(4, NodeKind::Step),
            node(5, NodeKind::Step),
        ];
        let edges = [edge(1, 2), edge(1, 3), edge(3, 4), edge(2, 5)];
        let layout = Layout::of(&nodes, &edges);
        let row = |id| layout.slots[&NodeId(id)].row;
        assert_eq!(row(5) < row(4), row(2) < row(3));
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
    fn a_tall_column_folds_into_a_block_keeping_the_layers_apart() {
        // 40 steps on the origin, and one more step after the first of them.
        let mut nodes = vec![node(1, NodeKind::Origin)];
        let mut edges = Vec::new();
        for id in 2..42 {
            nodes.push(node(id, NodeKind::Step));
            edges.push(edge(1, id));
        }
        nodes.push(node(42, NodeKind::Step));
        edges.push(edge(2, 42));
        let layout = Layout::of(&nodes, &edges);
        let slot = |id| layout.slots[&NodeId(id)];
        let xs: std::collections::BTreeSet<u32> = (2..42)
            .map(|id| (slot(id).x * 100.0).round() as u32)
            .collect();
        assert!(xs.len() > 1, "the column folds into lanes: {xs:?}");
        assert!(layout.rows <= FOLD_ROWS, "{} rows", layout.rows);
        let block_end = (2..42).map(|id| slot(id).x).fold(0.0_f32, f32::max);
        assert!(
            slot(42).x >= block_end + 1.0 - f32::EPSILON,
            "the next layer starts after the block"
        );
        assert!((layout.width - slot(42).x).abs() < f32::EPSILON);
    }

    #[test]
    fn siblings_are_ordered_by_importance_from_the_middle_out() {
        // Five steps on one origin: the running one in the middle, the
        // pruned one at an edge, higher priority nearer the middle.
        let mut nodes = vec![node(1, NodeKind::Origin)];
        let statuses = [
            (2, NodeStatus::Pruned, 0),
            (3, NodeStatus::Planned, 5),
            (4, NodeStatus::Running, 0),
            (5, NodeStatus::Planned, 50),
            (6, NodeStatus::Proposed, 0),
        ];
        for (id, status, priority) in statuses {
            let mut step = node(id, NodeKind::Step);
            step.status = status;
            step.priority = priority;
            nodes.push(step);
        }
        let edges: Vec<Edge> = (2..7).map(|id| edge(1, id)).collect();
        let layout = Layout::of(&nodes, &edges);
        let distance = |id| layout.slots[&NodeId(id)].row.abs();
        assert!(distance(4) < f32::EPSILON, "running in the middle");
        assert!(distance(5) <= distance(3), "priority 50 nearer than 5");
        assert!(distance(3) <= distance(6), "planned nearer than proposed");
        assert!(
            (2..7).all(|id| distance(2) >= distance(id)),
            "pruned at an edge"
        );
    }

    #[test]
    fn empty_graph_has_no_columns() {
        let layout = Layout::of(&[], &[]);
        assert_eq!(layout.columns, 0);
        assert!(layout.slots.is_empty());
    }
}
