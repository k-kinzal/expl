//! Where each node sits. Left to right is time; top to bottom sorts the
//! work into groups by how much it is believed in.
//!
//! - **Left to right is time.** A node's **column** is the length of the
//!   longest path reaching it, counting both what it builds on and, for a
//!   step on a planned path, the steps the planner put before it — so
//!   every edge points rightwards and a path reads in the order it is to
//!   be carried out. Goals are pinned to the columns after the deepest
//!   step: they are where everything heads. A subgoal is a stage on the
//!   way to the goals it serves, so it comes before them in time: the
//!   top-level goals share the last column, and each level of subgoals
//!   sits one column further left, so subgoals converge into their goals
//!   while separate top-level goals stand side by side.
//! - **The trunk is a straight line.** Every origin sits on row `0`; since
//!   each origin builds on the previous one and on what it adopted, origins
//!   march rightwards as the trunk advances, with history to the left.
//! - **Work comes in groups, one band each.** Each path of the latest plan
//!   is a group, of its steps ahead of the current origin (the rest is
//!   history). Steps grown past a path's step are a group of their own, by
//!   that step; a chain grown past any other step goes on in that step's
//!   group; and a step on an origin that others build on starts one.
//!   Each group is one horizontal band, its steps of one column stacked
//!   most promising innermost (wrapping into a block when there are many),
//!   and bands are kept apart by a gap wider than the spacing within one,
//!   so a group reads as one.
//! - **Nearer the line is more believed in.** Bands are placed in order —
//!   the planner's paths by the priority of their open steps, then chains
//!   with open steps by their most promising step, then finished ones — each in the free place nearest row
//!   `0`, alternately above and below. Up to the current origin row `0` is
//!   the trunk's; past it, the first band to fit continues the line toward
//!   the goals. Chains that do not overlap in time share a place; paths
//!   never share a row, since one after another would read as one path.
//! - **Steps on their own are a cloud beyond the bands.** A step on an
//!   origin that nothing builds on is an alternative not taken up yet.
//!   These gather beyond everything else in their column: the most
//!   promising nearest and the rest further out (pruned ones furthest),
//!   leaning forward in time. Which side is the stream
//!   ([`Streams`]): side streams go to their own side, the mainstream's
//!   (and a human's) alternate between both.
//!
//! All positions are in rows (the vertical spacing of the drawing); rows
//! are centred on `0`. The layout is pure and deterministic: the same graph
//! always draws the same way, so nodes only move when the graph changes.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

use expl_core::{Edge, GoalTree, Node, NodeId, NodeKind, NodeStatus, PlannedPath};

/// Horizontal room between one column's rightmost node and the next
/// column's start, in rows.
pub(crate) const COLUMN_GAP: f32 = 2.0;

/// The least distance between two nodes, in rows.
const SPACING: f32 = 1.0;

/// Bands are placed on half rows: steps of one band are two units apart,
/// and a band keeps one more unit clear on either side, so steps of
/// different bands are at least a row and a half apart.
const UNIT: f32 = 0.5;

/// How far apart in time two bands must be to share a place, in rows: more
/// than one column, so a band is never read as the next stage of another.
const BAND_GAP: f32 = 2.0 * COLUMN_GAP + 0.5;

/// The most rows one group takes in one column; more steps than that wrap
/// into a block.
const BAND_DEPTH: usize = 5;

/// Distance from the outermost band to the nearest step of a cloud, in
/// rows.
const CLOUD_GAP: f32 = 1.5;

/// How far a cloud leans forward from straight out, at most, in radians
/// (25°): narrow, so its column takes little room.
const CLOUD_LEAN: f32 = 0.436_332_3;

/// The steepest a stream branches off, in radians (70°).
const MAX_ANGLE: f32 = 1.221_730_5;

/// How far a crowded-out step moves on along its direction per try, in
/// rows.
const MARCH: f32 = 0.2;

/// How far a crowded-out step may turn either way per try, in radians (8°).
const TURN: f32 = 0.139_626_34;

/// How far the `n`-th most promising step of a cloud aims from where the
/// cloud starts: `REACH * √n` rows, so the cloud fills evenly.
const REACH: f32 = 0.8;

/// The golden ratio's fractional part: successive ranks spread over the
/// cloud's lean evenly and deterministically.
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

/// Where one group of steps was placed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Band {
    /// The goal, when the group is a path of the latest plan.
    pub(crate) goal: Option<NodeId>,
    /// Its steps.
    pub(crate) steps: Vec<NodeId>,
    /// Leftmost and rightmost `x`.
    pub(crate) x: (f32, f32),
    /// Top and bottom row.
    pub(crate) rows: (f32, f32),
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
    /// Every group's band, nearest the line first.
    pub(crate) bands: Vec<Band>,
    /// Number of columns.
    pub(crate) columns: usize,
    /// Horizontal extent: the largest `x` of any slot.
    pub(crate) width: f32,
}

/// The loose steps of one column: each with whether it goes above the
/// bands, and where it settled relative to the cloud's start.
type Cloud = Vec<(NodeId, bool, (f32, f32))>;

/// A group of steps before it is placed.
struct Group {
    goal: Option<NodeId>,
    steps: Vec<NodeId>,
}

impl Layout {
    /// Lay out `nodes` connected by `edges`, grouping steps by the latest
    /// plan's `paths` and the rest by what they build on; loose steps go to
    /// the side of their `streams`.
    #[must_use]
    pub(crate) fn of(
        nodes: &[Node],
        edges: &[Edge],
        paths: &[PlannedPath],
        streams: &Streams,
    ) -> Self {
        let by_id: HashMap<NodeId, &Node> = nodes.iter().map(|n| (n.id, n)).collect();
        let kind = |id: &NodeId| by_id.get(id).map(|n| n.kind);
        let is_step = |id: &NodeId| kind(id) == Some(NodeKind::Step);

        // What builds on what, goals aside (they are pinned last).
        let mut preds: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        let mut succs: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        let mut toward: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for edge in edges {
            match (kind(&edge.source), kind(&edge.target)) {
                (Some(NodeKind::Goal) | None, _) | (_, None) => {}
                (Some(_), Some(NodeKind::Goal)) => {
                    toward.entry(edge.target).or_default().push(edge.source);
                }
                (Some(_), Some(NodeKind::Origin | NodeKind::Step)) => {
                    preds.entry(edge.target).or_default().push(edge.source);
                    succs.entry(edge.source).or_default().push(edge.target);
                }
            }
        }
        let builds_on = preds.clone();

        // The planner's paths, most believed in first: by the highest
        // priority among their open steps (paths with none last), then in
        // the planner's order.
        let open = |id: &NodeId| {
            by_id.get(id).is_some_and(|n| {
                n.kind == NodeKind::Step
                    && matches!(
                        n.status,
                        NodeStatus::Proposed | NodeStatus::Planned | NodeStatus::Running
                    )
            })
        };
        let mut ranked: Vec<&PlannedPath> = paths.iter().collect();
        ranked.sort_by_key(|path| {
            Reverse(
                path.nodes
                    .iter()
                    .filter(|id| open(id))
                    .map(|id| by_id[id].priority)
                    .max(),
            )
        });
        // A path is carried out in its order: each step after the one
        // before it, unless what it builds on says otherwise.
        for path in &ranked {
            let steps: Vec<NodeId> = path.nodes.iter().copied().filter(is_step).collect();
            for pair in steps.windows(2) {
                let (before, after) = (pair[0], pair[1]);
                if before != after && !reaches(&succs, after, before) {
                    preds.entry(after).or_default().push(before);
                    succs.entry(before).or_default().push(after);
                }
            }
        }

        // Longest-path depth of every non-goal node (memoised DFS; the
        // `visiting` guard makes a malformed cycle harmless).
        let mut depth: HashMap<NodeId, usize> = HashMap::new();
        let mut visiting = Vec::new();
        for node in nodes.iter().filter(|n| n.kind != NodeKind::Goal) {
            depth_of(node.id, &preds, &mut depth, &mut visiting);
        }
        // Goals after every step: top-level goals last, each level of
        // subgoals one column before the goals it serves.
        let goal_tree = GoalTree::of(nodes, edges);
        let goal_depth: HashMap<NodeId, usize> = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Goal)
            .map(|n| (n.id, goal_tree.depth(n.id)))
            .collect();
        let deepest_goal = goal_depth.values().copied().max().unwrap_or(0);
        let goal_column = depth.values().max().map_or(0, |d| d + 1);
        let column_of = |id: &NodeId| match kind(id) {
            Some(NodeKind::Goal) => goal_column + deepest_goal - goal_depth[id],
            Some(NodeKind::Origin | NodeKind::Step) | None => depth[id],
        };

        // Groups: the paths — what lies ahead of the current origin; steps
        // of a path at or behind it are history — then chains.
        let trunk_column = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Origin)
            .map(|n| depth[&n.id])
            .max();
        let ahead = |id: &NodeId| trunk_column.is_none_or(|end| depth[id] > end);
        let mut group_of: HashMap<NodeId, usize> = HashMap::new();
        let mut groups: Vec<Group> = Vec::new();
        for path in &ranked {
            let mut steps = Vec::new();
            for id in path.nodes.iter().filter(|id| is_step(id) && ahead(id)) {
                if !group_of.contains_key(id) {
                    group_of.insert(*id, groups.len());
                    steps.push(*id);
                }
            }
            if !steps.is_empty() {
                groups.push(Group {
                    goal: Some(path.goal),
                    steps,
                });
            }
        }
        let planned = groups.len();
        let mut steps: Vec<NodeId> = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Step)
            .map(|n| n.id)
            .collect();
        steps.sort_by_key(|id| (depth[id], *id));
        let mut loose: Vec<NodeId> = Vec::new();
        // Chains grown past a path's step, by that step.
        let mut past: HashMap<NodeId, usize> = HashMap::new();
        for id in steps {
            if group_of.contains_key(&id) {
                continue;
            }
            let on: Vec<NodeId> = builds_on
                .get(&id)
                .into_iter()
                .flatten()
                .copied()
                .filter(is_step)
                .collect();
            // A chain goes on in its band; a path's band holds only what
            // the planner put on it.
            let joined = on
                .iter()
                .filter_map(|p| group_of.get(p))
                .filter(|g| **g >= planned)
                .min()
                .copied();
            if let Some(group) = joined {
                group_of.insert(id, group);
                groups[group].steps.push(id);
            } else if let Some(parent) = on.iter().min() {
                let group = *past.entry(*parent).or_insert_with(|| {
                    groups.push(Group {
                        goal: None,
                        steps: Vec::new(),
                    });
                    groups.len() - 1
                });
                group_of.insert(id, group);
                groups[group].steps.push(id);
            } else if succs.get(&id).is_some_and(|s| s.iter().any(is_step)) {
                group_of.insert(id, groups.len());
                groups.push(Group {
                    goal: None,
                    steps: vec![id],
                });
            } else {
                loose.push(id);
            }
        }
        // Chains still open first, so the work ahead is placed before
        // history; then by their most promising step.
        let best = |group: &Group| {
            let done = !group.steps.iter().any(&open);
            let most = group
                .steps
                .iter()
                .map(|id| promise(by_id[id]))
                .min()
                .unwrap_or((u8::MAX, Reverse(i64::MIN)));
            (done, most)
        };
        let mut order: Vec<usize> = (0..groups.len()).collect();
        order[planned..].sort_by_key(|g| (best(&groups[*g]), groups[*g].steps[0]));

        // Clouds, shaped relative to where each starts: `(dx, dy)` with `dy`
        // outward from the bands.
        let mut clouds: BTreeMap<usize, Cloud> = BTreeMap::new();
        {
            let mut by_column: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
            for id in &loose {
                by_column.entry(column_of(id)).or_default().push(*id);
            }
            for (column, mut ids) in by_column {
                ids.sort_by_key(|id| (promise(by_id[id]), *id));
                let mut sides = [Field::new(), Field::new()];
                let mut ranks = [0_usize; 2];
                let mut mainstream = 0_usize;
                let cloud = clouds.entry(column).or_default();
                for id in ids {
                    let angle = streams.angle(by_id[&id]);
                    let up = if angle < 0.0 {
                        true
                    } else if angle > 0.0 {
                        false
                    } else {
                        mainstream += 1;
                        mainstream % 2 == 1
                    };
                    let side = usize::from(!up);
                    let n = index_as_f32(ranks[side]);
                    ranks[side] += 1;
                    let reach = REACH * n.sqrt() * SPACING;
                    let lean = (n * GOLDEN).fract() * CLOUD_LEAN;
                    let at = sides[side].settle(reach, std::f32::consts::FRAC_PI_2 - lean);
                    cloud.push((id, up, at));
                }
            }
        }

        // Each group's steps by column; a column of many wraps into a block
        // at most `BAND_DEPTH` rows deep.
        let members: Vec<BTreeMap<usize, Vec<NodeId>>> = groups
            .iter()
            .map(|group| {
                let mut by_column: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
                for id in &group.steps {
                    by_column.entry(column_of(id)).or_default().push(*id);
                }
                by_column
            })
            .collect();
        let mut across: HashMap<usize, usize> = HashMap::new();
        for by_column in &members {
            for (column, ids) in by_column {
                let wide = across.entry(*column).or_insert(1);
                *wide = (*wide).max(wrap(ids.len()).0);
            }
        }

        // Columns take the room their clouds and blocks need.
        let mut columns: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
        for node in nodes {
            columns
                .entry(column_of(&node.id))
                .or_default()
                .push(node.id);
        }
        let mut starts: HashMap<usize, (f32, f32)> = HashMap::new();
        let mut start = 0.0_f32;
        for column in columns.keys() {
            let room = clouds
                .get(column)
                .into_iter()
                .flatten()
                .map(|(_, _, (dx, _))| *dx)
                .fold(0.0_f32, f32::max)
                .max(index_as_f32(across.get(column).map_or(0, |a| a - 1)) * SPACING);
            starts.insert(*column, (start, start + room));
            start += room + COLUMN_GAP;
        }
        let x_of = |id: &NodeId| starts[&column_of(id)].0;

        let mut slots: HashMap<NodeId, Slot> = HashMap::new();
        let at = |id: NodeId, x: f32, row: f32| Slot {
            column: column_of(&id),
            x,
            row,
        };

        // The trunk: origins on row 0, which is theirs up to the current one.
        let mut taken = Taken::default();
        let mut trunk_end = None;
        for node in nodes.iter().filter(|n| n.kind == NodeKind::Origin) {
            let x = x_of(&node.id);
            slots.insert(node.id, at(node.id, x, 0.0));
            trunk_end = Some(trunk_end.map_or(x, |end: f32| end.max(x)));
        }

        // Bands, most believed in first, each in the free place nearest
        // the line.
        let mut bands = Vec::new();
        for g in order {
            let group = &groups[g];
            let by_column = &members[g];
            let height = by_column
                .values()
                .map(|ids| wrap(ids.len()).1)
                .max()
                .unwrap_or(1);
            let span = 2 * i64::try_from(height).unwrap_or(i64::MAX / 4) - 1;
            let x = by_column
                .iter()
                .map(|(column, ids)| {
                    let start = starts[column].0;
                    (start, start + index_as_f32(wrap(ids.len()).0 - 1) * SPACING)
                })
                .fold((f32::MAX, f32::MIN), |(lo, hi), (a, b)| {
                    (lo.min(a), hi.max(b))
                });
            let reach = if group.goal.is_some() {
                (f32::MIN, f32::MAX)
            } else {
                (x.0 - BAND_GAP, x.1 + BAND_GAP)
            };
            // Past every unit taken so far, any place is free.
            let bound = taken.units.keys().map(|u| u.abs()).max().unwrap_or(0) + span + 2;
            let (low, high) = (0_i64..=bound)
                .flat_map(|i| [i, -i - 1])
                .map(|o| {
                    if o >= 0 {
                        (o, o + span - 1)
                    } else {
                        (o - span + 1, o)
                    }
                })
                .find(|(low, high)| {
                    (low - 1..=high + 1).all(|unit| {
                        taken.free(unit, reach)
                            && (unit != 0 || trunk_end.is_none_or(|end: f32| x.0 > end))
                    })
                })
                .unwrap_or((0, span - 1));
            for unit in low - 1..=high + 1 {
                taken.take(unit, x);
            }
            // Innermost is nearest the line.
            let (inner, outward) = if low >= 0 { (low, 2) } else { (high, -2) };
            for ids in by_column.values() {
                let mut ids = ids.clone();
                ids.sort_by_key(|id| (promise(by_id[id]), *id));
                let wide = wrap(ids.len()).0;
                for (i, id) in ids.into_iter().enumerate() {
                    let unit = inner + outward * i64::try_from(i / wide).unwrap_or(0);
                    let x = x_of(&id) + index_as_f32(i % wide) * SPACING;
                    slots.insert(id, at(id, x, half(unit)));
                }
            }
            bands.push(Band {
                goal: group.goal,
                steps: group.steps.clone(),
                x,
                rows: (half(low), half(high)),
            });
        }

        // Clouds, beyond every node placed within their column's room.
        for (column, cloud) in clouds {
            let room = starts[&column];
            let (top, bottom) = slots
                .values()
                .filter(|slot| room.0 - SPACING <= slot.x && slot.x <= room.1 + SPACING)
                .fold((0.0_f32, 0.0_f32), |(top, bottom), slot| {
                    (top.min(slot.row), bottom.max(slot.row))
                });
            for (id, up, (dx, dy)) in cloud {
                let row = if up {
                    top - CLOUD_GAP - dy
                } else {
                    bottom + CLOUD_GAP + dy
                };
                slots.insert(id, at(id, room.0 + dx, row));
            }
        }

        // Goals: one straight list per column, in the order of what leads
        // to them — the steps heading for them and, for a goal with
        // subgoals, the subgoals (placed first, since they lie further
        // left).
        let mut by_goal_column: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
        for node in nodes.iter().filter(|n| n.kind == NodeKind::Goal) {
            by_goal_column
                .entry(column_of(&node.id))
                .or_default()
                .push(node.id);
        }
        for goals in by_goal_column.values() {
            let mean_row = |id: &NodeId| {
                let rows: Vec<f32> = toward
                    .get(id)
                    .into_iter()
                    .flatten()
                    .copied()
                    .chain(goal_tree.children(*id))
                    .filter_map(|s| slots.get(&s).map(|slot| slot.row))
                    .collect();
                if rows.is_empty() {
                    0.0
                } else {
                    rows.iter().sum::<f32>() / index_as_f32(rows.len())
                }
            };
            let mut keyed: Vec<(f32, NodeId)> =
                goals.iter().map(|id| (mean_row(id), *id)).collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            let centre = index_as_f32(keyed.len().saturating_sub(1)) / 2.0;
            for (index, (_, id)) in keyed.into_iter().enumerate() {
                slots.insert(
                    id,
                    at(id, x_of(&id), (index_as_f32(index) - centre) * SPACING),
                );
            }
        }

        let width = slots.values().map(|s| s.x).fold(0.0_f32, f32::max);
        Self {
            columns: columns.keys().next_back().map_or(0, |c| c + 1),
            width,
            slots,
            bands,
        }
    }
}

/// Which half-row units are taken over which spans of `x`.
#[derive(Default)]
struct Taken {
    units: HashMap<i64, Vec<(f32, f32)>>,
}

impl Taken {
    fn take(&mut self, unit: i64, x: (f32, f32)) {
        self.units.entry(unit).or_default().push(x);
    }

    fn free(&self, unit: i64, x: (f32, f32)) -> bool {
        self.units
            .get(&unit)
            .is_none_or(|spans| spans.iter().all(|s| s.1 < x.0 || x.1 < s.0))
    }
}

/// The steps of one side of one cloud, settled so far, hashed by cell so a
/// new one only checks its neighbours. Coordinates are `(dx, dy)`: forward
/// in time and outward from the bands.
struct Field {
    cells: HashMap<(i64, i64), Vec<(f32, f32)>>,
}

impl Field {
    fn new() -> Self {
        Self {
            cells: HashMap::new(),
        }
    }

    fn cell((x, y): (f32, f32)) -> (i64, i64) {
        (
            quantise_to((x / SPACING).floor()),
            quantise_to((y / SPACING).floor()),
        )
    }

    fn free(&self, point: (f32, f32)) -> bool {
        if point.0 < -0.001 || point.1 < -0.001 {
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

    /// Settle a step aiming `reach` out at `angle` (from forward): on its
    /// aim when that is free, else at the first free point further out
    /// along its direction, turning a little either way as it goes — a
    /// step crowded out moves away, never ahead of steps placed before it.
    fn settle(&mut self, reach: f32, angle: f32) -> (f32, f32) {
        let mut out = 0.0_f32;
        loop {
            let r = reach + out;
            for turn in [0.0, -1.0, 1.0, -2.0, 2.0] {
                let a = angle + turn * TURN;
                let point = (r * a.cos(), r * a.sin());
                if self.free(point) {
                    self.cells.entry(Self::cell(point)).or_default().push(point);
                    return point;
                }
            }
            out += MARCH;
        }
    }
}

/// How `n` steps of one group in one column are arranged: `(across,
/// down)`, at most [`BAND_DEPTH`] down, most promising first along each row.
fn wrap(n: usize) -> (usize, usize) {
    let across = n.div_ceil(BAND_DEPTH).max(1);
    (across, n.div_ceil(across).max(1))
}

/// Whether `to` can be reached from `from` along `succs`.
fn reaches(succs: &HashMap<NodeId, Vec<NodeId>>, from: NodeId, to: NodeId) -> bool {
    let mut seen = HashSet::new();
    let mut stack = vec![from];
    while let Some(id) = stack.pop() {
        if id == to {
            return true;
        }
        if seen.insert(id) {
            stack.extend(succs.get(&id).into_iter().flatten().copied());
        }
    }
    false
}

/// How prominently a step is placed: smaller sits nearer the line.
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
        d = d.max(depth_of(source, preds, depth, visiting) + 1);
    }
    visiting.pop();
    depth.insert(id, d);
    d
}

/// A half-row unit as a row.
fn half(unit: i64) -> f32 {
    let clamped = i16::try_from(unit).unwrap_or(if unit < 0 { i16::MIN } else { i16::MAX });
    f32::from(clamped) * UNIT
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

    fn path(goal: i64, nodes: &[i64]) -> PlannedPath {
        PlannedPath {
            goal: NodeId(goal),
            nodes: nodes.iter().copied().map(NodeId).collect(),
            rationale: String::new(),
        }
    }

    fn layout(nodes: &[Node], edges: &[Edge]) -> Layout {
        Layout::of(nodes, edges, &[], &Streams::default())
    }

    fn assert_apart(layout: &Layout) {
        let slots: Vec<Slot> = layout.slots.values().copied().collect();
        for (i, a) in slots.iter().enumerate() {
            for b in &slots[i + 1..] {
                let apart = (a.x - b.x).hypot(a.row - b.row);
                assert!(apart > 0.99 * SPACING, "{a:?} and {b:?} overlap");
            }
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
    fn subgoals_come_before_the_goals_they_serve() {
        // Goal 2 ← subgoal 6 ← subsubgoal 7; goal 8 stands alone beside 2.
        // 1 → 3 → 4 toward 7, 1 → 5 toward 2 directly.
        let nodes = [
            node(1, NodeKind::Origin),
            node(2, NodeKind::Goal),
            node(3, NodeKind::Step),
            node(4, NodeKind::Step),
            node(5, NodeKind::Step),
            node(6, NodeKind::Goal),
            node(7, NodeKind::Goal),
            node(8, NodeKind::Goal),
        ];
        let edges = [
            edge(1, 3),
            edge(3, 4),
            edge(4, 7),
            edge(1, 5),
            edge(5, 2),
            edge(6, 2),
            edge(7, 6),
            edge(5, 8),
        ];
        let layout = layout(&nodes, &edges);
        let column = |id| layout.slots[&NodeId(id)].column;
        assert_eq!(column(4), 2);
        assert_eq!(column(7), 3, "the deepest subgoal right after the steps");
        assert_eq!(column(6), 4, "a subgoal before the goal it serves");
        assert_eq!(column(2), 5, "top-level goals last");
        assert_eq!(column(8), 5, "top-level goals side by side");
        for e in &edges {
            let (s, t) = (layout.slots[&e.source], layout.slots[&e.target]);
            assert!(s.x < t.x, "{e:?}: {s:?} → {t:?}");
        }
        assert_apart(&layout);
    }

    #[test]
    fn every_edge_points_right_and_no_two_nodes_overlap() {
        let mut nodes = vec![node(1, NodeKind::Origin), node(2, NodeKind::Goal)];
        let mut edges = Vec::new();
        for id in 3..80 {
            let by = ["explorer", "fringe", "surveyor"][usize::try_from(id % 3).expect("id")];
            nodes.push(step(id, NodeStatus::Planned, id % 7, Some(by)));
            // A few steps build on earlier ones, the rest on the origin.
            edges.push(edge(if id % 5 == 0 { id - 2 } else { 1 }, id));
            edges.push(edge(id, 2));
        }
        // A path whose order runs against what its steps build on.
        let paths = [path(2, &[13, 11, 30, 4])];
        let streams = Streams::of(&[("explorer", 4), ("fringe", 1), ("surveyor", 1)]);
        let layout = Layout::of(&nodes, &edges, &paths, &streams);
        for e in &edges {
            let (s, t) = (layout.slots[&e.source], layout.slots[&e.target]);
            assert!(s.x < t.x, "{e:?}: {s:?} → {t:?}");
        }
        assert_apart(&layout);
        assert_eq!(layout, Layout::of(&nodes, &edges, &paths, &streams));
    }

    #[test]
    fn a_planned_path_is_one_strand_in_its_order() {
        // Three steps on the origin, planned to run 5, 3, 4.
        let mut nodes = vec![node(1, NodeKind::Origin), node(2, NodeKind::Goal)];
        for id in 3..=5 {
            nodes.push(step(id, NodeStatus::Planned, 10, None));
        }
        let edges = [edge(1, 3), edge(1, 4), edge(1, 5), edge(4, 2)];
        let layout = Layout::of(&nodes, &edges, &[path(2, &[5, 3, 4])], &Streams::default());
        let slot = |id| layout.slots[&NodeId(id)];
        assert!(
            slot(5).x < slot(3).x && slot(3).x < slot(4).x,
            "in its order"
        );
        for id in [3, 4, 5] {
            assert!(
                slot(id).row.abs() < f32::EPSILON,
                "#{id} continues the line"
            );
        }
        assert_eq!(layout.bands.len(), 1);
        assert_eq!(layout.bands[0].goal, Some(NodeId(2)));
    }

    #[test]
    fn bands_nearer_the_line_are_believed_in_more() {
        // Origin 1. Path A (high priority) 3 → 4, path B (low) 5 → 6,
        // and a chain nobody planned, 7 → 8.
        let mut nodes = vec![node(1, NodeKind::Origin), node(2, NodeKind::Goal)];
        nodes.push(step(3, NodeStatus::Planned, 100, None));
        nodes.push(step(4, NodeStatus::Planned, 90, None));
        nodes.push(step(5, NodeStatus::Planned, 50, None));
        nodes.push(step(6, NodeStatus::Planned, 40, None));
        nodes.push(step(7, NodeStatus::Planned, 70, None));
        nodes.push(step(8, NodeStatus::Planned, 60, None));
        let edges = [
            edge(1, 3),
            edge(3, 4),
            edge(1, 5),
            edge(5, 6),
            edge(1, 7),
            edge(7, 8),
        ];
        // The planner lists the weaker path first: priority decides.
        let paths = [path(2, &[5, 6]), path(2, &[3, 4])];
        let layout = Layout::of(&nodes, &edges, &paths, &Streams::default());
        let row = |id| layout.slots[&NodeId(id)].row.abs();
        assert!(row(3) < row(5), "the stronger path nearer the line");
        assert!(row(5) <= row(7), "planned paths before unplanned chains");
        assert!((row(3) - row(4)).abs() < f32::EPSILON, "one band, one row");
        assert!(
            (row(7) - row(8)).abs() < f32::EPSILON,
            "a chain is a band too"
        );
        for (a, b) in [(3, 5), (3, 7), (5, 7)] {
            let gap = (layout.slots[&NodeId(a)].row - layout.slots[&NodeId(b)].row).abs();
            assert!(gap >= 1.5, "bands #{a} and #{b} kept apart: {gap}");
        }
        assert_eq!(layout.bands.len(), 3);
        assert_eq!(layout.bands[2].goal, None);
    }

    #[test]
    fn steps_grown_past_a_path_are_a_band_of_their_own() {
        let mut nodes = vec![node(1, NodeKind::Origin), node(2, NodeKind::Goal)];
        for id in 3..=7 {
            nodes.push(step(id, NodeStatus::Planned, 10, None));
        }
        // 3 → 4 planned; 5 and 6 grow past 4; 7 is loose.
        let edges = [edge(1, 3), edge(3, 4), edge(4, 5), edge(4, 6), edge(1, 7)];
        let layout = Layout::of(&nodes, &edges, &[path(2, &[3, 4])], &Streams::default());
        assert_eq!(layout.bands.len(), 2);
        assert_eq!(layout.bands[0].steps, vec![NodeId(3), NodeId(4)]);
        assert_eq!(layout.bands[1].steps, vec![NodeId(5), NodeId(6)]);
        assert_eq!(layout.bands[1].goal, None);
    }

    #[test]
    fn loose_steps_gather_beyond_the_bands_by_stream_and_promise() {
        let streams = Streams::of(&[("explorer", 4), ("fringe", 1), ("surveyor", 1)]);
        let mut nodes = vec![node(1, NodeKind::Origin), node(2, NodeKind::Goal)];
        // A planned path 3 → 4, and 60 loose steps on the origin.
        nodes.push(step(3, NodeStatus::Planned, 100, None));
        nodes.push(step(4, NodeStatus::Planned, 100, None));
        let mut edges = vec![edge(1, 3), edge(3, 4)];
        let by = ["explorer", "fringe", "surveyor"];
        for id in 10..70 {
            let agent = by[usize::try_from(id % 3).expect("id")];
            let status = if id >= 64 {
                NodeStatus::Pruned
            } else {
                NodeStatus::Planned
            };
            nodes.push(step(id, status, 1000 - id, Some(agent)));
            edges.push(edge(1, id));
        }
        let layout = Layout::of(&nodes, &edges, &[path(2, &[3, 4])], &streams);
        assert_apart(&layout);
        let band = &layout.bands[0];
        let rows = |agent: &str| -> Vec<f32> {
            nodes
                .iter()
                .filter(|n| n.proposed_by.as_deref() == Some(agent))
                .map(|n| layout.slots[&n.id].row)
                .collect()
        };
        assert!(
            rows("fringe").iter().all(|r| *r < band.rows.0),
            "fringe above"
        );
        assert!(
            rows("surveyor").iter().all(|r| *r > band.rows.1),
            "surveyor below"
        );
        let main = rows("explorer");
        assert!(main.iter().any(|r| *r < band.rows.0) && main.iter().any(|r| *r > band.rows.1));

        // Nearer the bands is more promising; pruned furthest.
        let out = |id: i64| {
            let row = layout.slots[&NodeId(id)].row;
            if row < band.rows.0 {
                band.rows.0 - row
            } else {
                row - band.rows.1
            }
        };
        let mean = |v: &[f32]| v.iter().sum::<f32>() / index_as_f32(v.len());
        let first: Vec<f32> = (10..30).map(out).collect();
        let later: Vec<f32> = (40..64).map(out).collect();
        assert!(mean(&first) < mean(&later), "higher priority nearer");
        let mut open: Vec<f32> = (10..64).map(out).collect();
        open.sort_by(f32::total_cmp);
        for id in 64..70 {
            assert!(out(id) > open[open.len() / 2], "pruned #{id} far out");
        }
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
        // 1 → 3 → 4, 1 → 5; origin 6 adopts 4; 6 → 7 → 8 → 2.
        let nodes = [
            node(1, NodeKind::Origin),
            node(2, NodeKind::Goal),
            step(3, NodeStatus::Completed, 0, None),
            step(4, NodeStatus::Completed, 0, None),
            step(5, NodeStatus::Pruned, 0, None),
            node(6, NodeKind::Origin),
            step(7, NodeStatus::Planned, 0, None),
            step(8, NodeStatus::Planned, 0, None),
        ];
        let edges = [
            edge(1, 3),
            edge(3, 4),
            edge(1, 5),
            edge(1, 6),
            edge(4, 6),
            edge(6, 7),
            edge(7, 8),
            edge(8, 2),
        ];
        let layout = layout(&nodes, &edges);
        let slot = |id| layout.slots[&NodeId(id)];
        assert_eq!(slot(6).column, 3, "an origin comes after what it adopted");
        for origin in [1, 6] {
            assert!(slot(origin).row.abs() < f32::EPSILON, "origins on the line");
        }
        for step in [3, 4, 5] {
            assert!(slot(step).row.abs() >= 1.0, "#{step} clear of the trunk");
        }
        // Past the current origin, the first band continues the line.
        assert!(slot(7).row.abs() < f32::EPSILON && slot(8).row.abs() < f32::EPSILON);
        assert_eq!(slot(2).column, 6);
    }

    #[test]
    fn empty_graph_has_no_columns() {
        let layout = layout(&[], &[]);
        assert_eq!(layout.columns, 0);
        assert!(layout.slots.is_empty());
    }
}
