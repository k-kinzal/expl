//! Where each node sits: a layered drawing of the DAG, left to right, in
//! which the position of a step says how it stands.
//!
//! - A node's **column** is the length of the longest path reaching it, so
//!   every edge points rightwards. Goals are pinned to one column after the
//!   deepest step: they are where everything heads. Columns take the room
//!   their nodes need; each starts [`COLUMN_GAP`] after the previous one
//!   ends.
//! - **The trunk is a straight line.** Every origin sits on row `0` at the
//!   start of its column; since each origin builds on the previous one and
//!   on what it adopted, origins march rightwards as the trunk advances,
//!   with history to the left. Up to the current origin, other nodes keep
//!   clear of row `0` so the trunk line between origins is never covered.
//! - **Steps fan out from what they build on.** Each step aims for a point
//!   measured from its *anchor*: the start of its column, on the mean row
//!   of its predecessors.
//!   - **Near or far is promise.** Among the steps sharing an anchor, the
//!     most promising (running, then completed or failed work, then planned
//!     steps by priority, then proposals) sits closest, on the anchor's row;
//!     the rest lie further out in order, pruned steps furthest. The
//!     distance grows with the square root of the rank, so the fan fills
//!     evenly.
//!   - **The direction is the stream.** The exploration agent with the
//!     largest routing weight is the mainstream: its proposals, and steps a
//!     human added, head straight on. Agents with a smaller share branch
//!     off, the smaller the share the steeper, alternately above and below
//!     ([`Streams`]).
//!
//!   Steps settle most promising first, each on its aim or, when that is
//!   taken, further out along its own direction, so nothing overlaps, a
//!   crowded-out step only ever moves away, and no rows or columns form.
//!
//! All positions are in rows (the vertical spacing of the drawing); rows
//! are centred on `0`. The layout is pure and deterministic: the same graph
//! always draws the same way, so nodes only move when the graph changes.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};

use expl_core::{Edge, Node, NodeId, NodeKind, NodeStatus};

/// Horizontal room between one column's rightmost node and the next
/// column's start, in rows.
pub(crate) const COLUMN_GAP: f32 = 200.0 / 56.0;

/// The least distance between two nodes, in rows.
const SPACING: f32 = 1.0;

/// How far a crowded-out step moves on along its direction per try, in
/// rows.
const MARCH: f32 = 0.2;

/// How far a crowded-out step may turn either way per try, in radians (8°);
/// two turns at most, so it stays within 86° of straight on.
const TURN: f32 = 0.139_626_34;

/// How far the `n`-th most promising step aims from its anchor:
/// `REACH * √n` rows, so the fan fills evenly: the area within a distance
/// grows with its square.
const REACH: f32 = 0.8;

/// The steepest a stream branches off, in radians (70°).
const MAX_ANGLE: f32 = 1.221_730_5;

/// How far either way of its stream's direction a step may aim, in
/// radians (±30°): steps of one stream spread over an arc, so the steps of
/// one rank sit at one distance instead of queueing along a ray.
const SPREAD: f32 = std::f32::consts::FRAC_PI_6;

/// The golden ratio's fractional part: successive ranks spread over the arc
/// evenly and deterministically.
const GOLDEN: f32 = 0.618_034;

/// A node's place in the drawing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Slot {
    /// Column (layer), from the left.
    pub(crate) column: usize,
    /// Horizontal position, in rows from the left edge.
    pub(crate) x: f32,
    /// Vertical position, in rows; the trunk is row `0`.
    pub(crate) row: f32,
}

/// Which way the proposals of each exploration agent head.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Streams {
    /// Angle from straight on, by agent; negative is up.
    angles: HashMap<String, f32>,
}

impl Streams {
    /// Streams of exploration agents declared with these routing weights
    /// (`(name, weight)`, in declaration order). The heaviest are the
    /// mainstream and head straight on; the others branch off by
    /// `(1 − weight / heaviest) · 70°`, alternately up and down in order,
    /// a further 10° out for each one already on that side.
    #[must_use]
    pub(crate) fn of(agents: &[(&str, u32)]) -> Self {
        let heaviest = agents.iter().map(|(_, w)| *w).max().unwrap_or(1).max(1);
        let mut sides = [0_u8; 2];
        let mut angles = HashMap::new();
        for (name, weight) in agents {
            let share = f32::from(u16::try_from(*weight).unwrap_or(u16::MAX))
                / f32::from(u16::try_from(heaviest).unwrap_or(u16::MAX));
            if *weight >= heaviest {
                angles.insert((*name).to_owned(), 0.0);
                continue;
            }
            let side = usize::from(sides[0] > sides[1]);
            let crowd = f32::from(sides[side]) * 10.0_f32.to_radians();
            sides[side] += 1;
            let angle = ((1.0 - share) * MAX_ANGLE + crowd).min(MAX_ANGLE);
            angles.insert((*name).to_owned(), if side == 0 { -angle } else { angle });
        }
        Self { angles }
    }

    /// The direction `node` heads: its proposer's stream, or straight on
    /// for a step a human added (or an agent no longer declared).
    fn angle(&self, node: &Node) -> f32 {
        node.proposed_by
            .as_deref()
            .and_then(|agent| self.angles.get(agent))
            .copied()
            .unwrap_or(0.0)
    }
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
}

impl Layout {
    /// Lay out `nodes` connected by `edges`, steps heading along `streams`.
    #[must_use]
    pub(crate) fn of(nodes: &[Node], edges: &[Edge], streams: &Streams) -> Self {
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
        let mut start = 0.0_f32;
        let mut width = 0.0_f32;
        for (&column, ids) in &columns {
            let mut field = Field {
                start,
                keep_clear: trunk_end.is_some_and(|end| column < end),
                cells: HashMap::new(),
            };
            let mut right = start;
            let goals = ids.iter().all(|id| kinds[id] == NodeKind::Goal);
            if goals {
                // Goals: one straight list, in the order of what leads to them.
                let mut ids = ids.clone();
                ids.sort_by(|a, b| {
                    barycentre(*a, &preds, &slots)
                        .total_cmp(&barycentre(*b, &preds, &slots))
                        .then(a.cmp(b))
                });
                let centre = index_as_f32(ids.len().saturating_sub(1)) / 2.0;
                for (index, id) in ids.iter().enumerate() {
                    let row = (index_as_f32(index) - centre) * SPACING;
                    slots.insert(
                        *id,
                        Slot {
                            column,
                            x: start,
                            row,
                        },
                    );
                }
            } else {
                let mut steps = Vec::new();
                for id in ids {
                    if kinds[id] == NodeKind::Origin {
                        field.place((start, 0.0));
                        slots.insert(
                            *id,
                            Slot {
                                column,
                                x: start,
                                row: 0.0,
                            },
                        );
                    } else {
                        steps.push(*id);
                    }
                }
                // Rank each step among those sharing its anchor.
                let anchor: HashMap<NodeId, f32> = steps
                    .iter()
                    .map(|id| (*id, barycentre(*id, &preds, &slots)))
                    .collect();
                steps.sort_by(|a, b| promise(by_id[a]).cmp(&promise(by_id[b])).then(a.cmp(b)));
                let mut ranks: HashMap<i64, usize> = HashMap::new();
                for id in &steps {
                    let key = quantise(anchor[id]);
                    let rank = ranks.entry(key).or_insert(0);
                    let n = index_as_f32(*rank);
                    *rank += 1;
                    let reach = REACH * n.sqrt() * SPACING;
                    let spread = ((n * GOLDEN).fract() - 0.5) * 2.0 * SPREAD;
                    let angle = (streams.angle(by_id[id]) + spread).clamp(-MAX_ANGLE, MAX_ANGLE);
                    let (x, row) = field.settle(anchor[id], reach, angle);
                    right = right.max(x);
                    slots.insert(*id, Slot { column, x, row });
                }
            }
            width = width.max(right);
            start = right + COLUMN_GAP;
        }

        Self {
            columns: columns.keys().next_back().map_or(0, |c| c + 1),
            width,
            slots,
        }
    }
}

/// The points already placed in one column, hashed by cell so a new one
/// only checks its neighbours.
struct Field {
    start: f32,
    /// Nothing near row `0`: the trunk line runs through this column.
    keep_clear: bool,
    cells: HashMap<(i64, i64), Vec<(f32, f32)>>,
}

impl Field {
    fn cell((x, row): (f32, f32)) -> (i64, i64) {
        (
            quantise_to((x / SPACING).floor()),
            quantise_to((row / SPACING).floor()),
        )
    }

    fn place(&mut self, point: (f32, f32)) {
        self.cells.entry(Self::cell(point)).or_default().push(point);
    }

    fn free(&self, point: (f32, f32)) -> bool {
        if point.0 < self.start - 0.001 || self.keep_clear && point.1.abs() < SPACING / 2.0 {
            return false;
        }
        let (i, j) = Self::cell(point);
        (i - 1..=i + 1).all(|ci| {
            (j - 1..=j + 1).all(|cj| {
                self.cells.get(&(ci, cj)).is_none_or(|placed| {
                    placed
                        .iter()
                        .all(|p| (p.0 - point.0).hypot(p.1 - point.1) >= SPACING * 0.999)
                })
            })
        })
    }

    /// Settle a step aiming `reach` out from `(start, anchor)` at `angle`:
    /// on its aim when that is free, else at the first free point further
    /// out along its direction, turning a little either way as it goes —
    /// a step crowded out moves away, never ahead of steps placed before it.
    fn settle(&mut self, anchor: f32, reach: f32, angle: f32) -> (f32, f32) {
        let mut out = 0.0_f32;
        loop {
            let r = reach + out;
            for turn in [0.0, 1.0, -1.0, 2.0, -2.0] {
                let a = angle + turn * TURN;
                let point = (self.start + r * a.cos(), anchor + r * a.sin());
                if self.free(point) {
                    self.place(point);
                    return point;
                }
            }
            out += MARCH;
        }
    }
}

/// How prominently a step is placed: smaller sits nearer its anchor.
fn promise(node: &Node) -> (u8, Reverse<i64>) {
    let class = match node.status {
        NodeStatus::Running => 0,
        NodeStatus::Completed | NodeStatus::Failed => 1,
        NodeStatus::Planned => 2,
        NodeStatus::Proposed => 3,
        NodeStatus::Active | NodeStatus::Superseded => 4,
        NodeStatus::Pruned => 5,
    };
    (class, Reverse(node.priority))
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

/// Mean row of the already-placed predecessors of `id`; `0` (the trunk)
/// for a node with none.
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

/// Anchors closer than a hundredth of a row are the same anchor.
fn quantise(row: f32) -> i64 {
    quantise_to(row * 100.0)
}

/// The nearest integer; the drawing never comes near `i64`'s range.
fn quantise_to(value: f32) -> i64 {
    value.round() as i64
}

/// Ranks and counts are small; saturating at `u16::MAX` is harmless.
fn index_as_f32(index: usize) -> f32 {
    f32::from(u16::try_from(index).unwrap_or(u16::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            proposed_by: None,
            branch: None,
            head_commit: None,
            outcome: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn step(id: i64, status: NodeStatus, priority: i64, by: Option<&str>) -> Node {
        let mut step = node(id, NodeKind::Step);
        step.status = status;
        step.priority = priority;
        step.proposed_by = by.map(str::to_owned);
        step
    }

    fn edge(source: i64, target: i64) -> Edge {
        Edge {
            source: NodeId(source),
            target: NodeId(target),
        }
    }

    fn layout(nodes: &[Node], edges: &[Edge]) -> Layout {
        Layout::of(nodes, edges, &Streams::default())
    }

    /// Distance of `id` from where the fan of `column` starts on row `row`.
    fn reach(layout: &Layout, id: i64, start: f32, row: f32) -> f32 {
        let slot = layout.slots[&NodeId(id)];
        (slot.x - start).hypot(slot.row - row)
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
        let layout = layout(&nodes, &edges);
        let column = |id| layout.slots[&NodeId(id)].column;
        assert_eq!(column(1), 0);
        assert_eq!(column(3), 1);
        assert_eq!(column(5), 1);
        assert_eq!(column(4), 2, "a merge sits after its deepest source");
        assert_eq!(column(2), 3, "goals sit after the deepest step");
        assert_eq!(layout.columns, 4);
    }

    #[test]
    fn every_edge_points_right_and_no_two_nodes_overlap() {
        let mut nodes = vec![node(1, NodeKind::Origin), node(2, NodeKind::Goal)];
        let mut edges = Vec::new();
        for id in 3..60 {
            nodes.push(step(id, NodeStatus::Planned, id % 7, None));
            // A few steps build on earlier ones, the rest on the origin.
            edges.push(edge(if id % 5 == 0 { id - 2 } else { 1 }, id));
            edges.push(edge(id, 2));
        }
        let layout = layout(&nodes, &edges);
        for e in &edges {
            let (s, t) = (layout.slots[&e.source], layout.slots[&e.target]);
            assert!(s.x < t.x, "{e:?}: {s:?} → {t:?}");
        }
        let slots: Vec<Slot> = layout.slots.values().copied().collect();
        for (i, a) in slots.iter().enumerate() {
            for b in &slots[i + 1..] {
                let apart = (a.x - b.x).hypot(a.row - b.row);
                assert!(apart > 0.99 * SPACING, "{a:?} and {b:?} overlap");
            }
        }
        assert_eq!(layout, Layout::of(&nodes, &edges, &Streams::default()));
    }

    #[test]
    fn promise_sets_how_near_a_step_sits() {
        // 30 steps on one origin: one running, planned ones by priority,
        // proposals, and pruned ones.
        let mut nodes = vec![node(1, NodeKind::Origin)];
        nodes.push(step(2, NodeStatus::Running, 0, None));
        for id in 3..23 {
            nodes.push(step(id, NodeStatus::Planned, 1000 - id, None));
        }
        for id in 23..27 {
            nodes.push(step(id, NodeStatus::Proposed, 0, None));
        }
        for id in 27..32 {
            nodes.push(step(id, NodeStatus::Pruned, 5000, None));
        }
        let edges: Vec<Edge> = (2..32).map(|id| edge(1, id)).collect();
        let layout = layout(&nodes, &edges);
        let start = layout.slots[&NodeId(2)].x;
        let reach = |id| reach(&layout, id, start, 0.0);

        assert!(reach(2) < f32::EPSILON, "the running step on the line");
        let planned: Vec<f32> = (3..23).map(reach).collect();
        let (near, far) = planned.split_at(10);
        let mean = |v: &[f32]| v.iter().sum::<f32>() / index_as_f32(v.len());
        assert!(mean(near) < mean(far), "higher priority nearer");
        let mut open: Vec<f32> = (2..27).map(reach).collect();
        open.sort_by(f32::total_cmp);
        let pruned: Vec<f32> = (27..32).map(reach).collect();
        for (id, r) in (27..32).zip(&pruned) {
            assert!(
                *r > open[open.len() / 2],
                "pruned #{id} beyond most open steps"
            );
        }
        assert!(
            mean(&pruned) > mean(&open[open.len() / 2..]),
            "pruned at the edge"
        );
    }

    #[test]
    fn the_stream_sets_which_way_a_step_heads() {
        let streams = Streams::of(&[("explorer", 4), ("fringe", 1), ("surveyor", 1)]);
        let mut nodes = vec![node(1, NodeKind::Origin)];
        let by = ["explorer", "fringe", "surveyor"];
        for id in 2..62 {
            let agent = by[usize::try_from(id).expect("id") % 3];
            nodes.push(step(id, NodeStatus::Planned, 10, Some(agent)));
        }
        let edges: Vec<Edge> = (2..62).map(|id| edge(1, id)).collect();
        let layout = Layout::of(&nodes, &edges, &streams);
        let start = layout.slots[&NodeId(1)].x + COLUMN_GAP;
        let heading = |agent: &str| {
            let rows: Vec<f32> = nodes
                .iter()
                .filter(|n| n.proposed_by.as_deref() == Some(agent))
                .map(|n| {
                    let slot = layout.slots[&n.id];
                    slot.row.atan2(slot.x - start)
                })
                .collect();
            rows.iter().sum::<f32>() / index_as_f32(rows.len())
        };
        let (main, up, down) = (heading("explorer"), heading("fringe"), heading("surveyor"));
        assert!(main.abs() < 0.35, "mainstream heads on: {main}");
        assert!(up < -0.5, "one side stream above: {up}");
        assert!(down > 0.5, "the other below: {down}");
    }

    #[test]
    fn streams_branch_off_by_their_share() {
        let streams = Streams::of(&[("a", 4), ("b", 1), ("c", 2), ("d", 1), ("e", 4)]);
        let angle = |name: &str| {
            let mut n = node(9, NodeKind::Step);
            n.proposed_by = Some(name.to_owned());
            streams.angle(&n)
        };
        assert!(angle("a").abs() < f32::EPSILON && angle("e").abs() < f32::EPSILON);
        assert!(angle("b") < 0.0 && angle("c") > 0.0, "alternate sides");
        assert!(
            angle("c").abs() < angle("b").abs(),
            "a larger share branches less"
        );
        assert!(
            angle("d").abs() > angle("b").abs(),
            "a second on one side further out"
        );
        assert!(angle("unknown").abs() < f32::EPSILON, "humans head on");
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
        let layout = layout(&nodes, &edges);
        let slot = |id| layout.slots[&NodeId(id)];
        assert_eq!(slot(6).column, 3, "an origin comes after what it adopted");
        for origin in [1, 6] {
            assert!(slot(origin).row.abs() < f32::EPSILON, "origins on the line");
        }
        for step in [3, 4, 5] {
            assert!(slot(step).row.abs() >= 0.5, "#{step} clear of the trunk");
        }
        // Past the current origin, its first child continues the line.
        assert!(slot(7).row.abs() < f32::EPSILON || slot(8).row.abs() < f32::EPSILON);
        assert_eq!(slot(2).column, 5);
    }

    #[test]
    fn empty_graph_has_no_columns() {
        let layout = layout(&[], &[]);
        assert_eq!(layout.columns, 0);
        assert!(layout.slots.is_empty());
    }
}
