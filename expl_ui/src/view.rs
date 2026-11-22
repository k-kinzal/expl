//! [`ExplView`] — the window: the graph, the trunk it grows from, and the
//! goals it is judged by.
//!
//! - **Nodes are dots.** State is brightness and shape: a hollow ring is
//!   waiting (proposed, planned), a solid dot is realised, a rippling dot is
//!   running, a speck is pruned. Goals are larger rings on the right.
//!   Amber marks what satisfies a goal; red marks what failed.
//! - **The trunk is the spine.** Origins — every position the default
//!   branch has held — sit on one straight line, oldest on the left. The
//!   current origin is the bright one with a ring: exploration grows from
//!   it. Work adopted into the trunk reads like a merged branch: its steps
//!   turn square, the line from the origin it was explored from through
//!   them is drawn in the trunk's colour, and it joins the spine at the
//!   origin that took it in with a line as strong as the spine itself. An
//!   origin that took work in is a merge: it carries a halo, and its label
//!   counts the steps it merged. Steps explored from an origin the trunk
//!   has moved past fade, unless they are still candidates.
//! - **Edges are hairlines.** Edges into goals are drawn only once a step
//!   satisfies the goal (or the step is selected), so the picture shows
//!   reached goals rather than intentions. A faint red line joins a failed
//!   step to each new hypothesis grown from its failure.
//! - **Goals** are listed top right, with where the trunk stands on each
//!   and the best candidate not adopted yet, subgoals indented under the
//!   goals they serve (and joined to them by a faint line in the graph).
//!   Selecting a goal focuses the graph on it: what leads to it, and what
//!   was judged against it.
//! - **The trunk's history** is listed bottom right: each origin, newest
//!   first, with what was adopted into it.
//! - **Agents are dots** bottom left, one per process, grouped by agent:
//!   pulsing while reacting, dim while idle, a ring for a missing replica,
//!   red when dead.
//! - Clicking a node or a listed goal or origin shows its words top left;
//!   Escape hides them.
//!
//! The view polls the database on a background thread and re-renders only
//! when the snapshot changed. Dragging pans; scrolling zooms the graph
//! around the pointer, `+` / `-` zoom around the window centre,
//! `f` fits the whole graph in the window, and `0` resets pan and zoom.
//! Zooming scales the distances between dots, not the dots or the panels;
//! when zoomed far out, only the labels that matter (selected, current,
//! running, candidates) stay.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use expl_core::{Epoch, Node, NodeId, NodeKind, NodeStatus, ProcessRecord, ProcessState, Project};
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, ElementId, FocusHandle, Hsla, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels,
    Point, Render, Rgba, ScrollWheelEvent, Size, Task, Window, canvas, div, point, prelude::*, px,
};

use crate::layout::{Layout, Streams};
use crate::snapshot::{Snapshot, SnapshotError};
use crate::theme;

/// Distance between columns and rows, dot centre to dot centre.
const ROW_STRIDE: f32 = 56.0;
/// Smallest left margin of the first column.
const MARGIN: f32 = 120.0;
/// Right margin once the graph is wider than the window: the newest
/// columns (the present, and the goals) stay in view.
const RIGHT_MARGIN: f32 = 150.0;
/// The square every dot is centred in.
const DOT_BOX: f32 = 16.0;
/// Width reserved for a label under a dot.
const LABEL_W: f32 = 172.0;
/// One trip of the comet along an edge into a running step.
const COMET: Duration = Duration::from_millis(1400);
/// Movement beyond this is a drag, not a click.
const CLICK_SLOP: f32 = 4.0;
/// Corner panels.
const DETAIL_W: f32 = 360.0;
const GOALS_W: f32 = 290.0;
/// How far each level of subgoals is indented in the goals panel.
const SUBGOAL_INDENT: f32 = 14.0;
const TRUNK_W: f32 = 320.0;
/// Origins listed in the trunk panel.
const TRUNK_ROWS: usize = 6;
/// Zoom range, the factor of one `+` / `-`, and the zoom below which only
/// labels that matter are drawn.
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 3.0;
const ZOOM_STEP: f32 = 1.25;
const QUIET_LABELS_BELOW: f32 = 0.55;
/// Narrowest label once zoomed out.
const MIN_LABEL_W: f32 = 72.0;
/// Room `f` leaves around the fitted graph for the corner panels.
const FIT_INSET_X: f32 = 120.0;
const FIT_INSET_Y: f32 = 110.0;
/// Opacity of nodes outside a focused goal, and of steps left behind.
const OUT_OF_FOCUS: f32 = 0.16;
const BEHIND: f32 = 0.45;

const MONO: &str = "Menlo";

/// See the [module docs](self).
pub(crate) struct ExplView {
    focus: FocusHandle,
    snapshot: Snapshot,
    layout: Layout,
    loaded: bool,
    error: Option<String>,
    selected: Option<NodeId>,
    pan: Point<Pixels>,
    /// Scale of the distances between dots (1.0: as laid out).
    zoom: f32,
    drag: Option<Drag>,
    pressed: Option<NodeId>,
    /// The press came from a panel: bring the node into view on select.
    reveal: bool,
    epoch: Instant,
    _poll: Task<()>,
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    last: Point<Pixels>,
    travelled: f32,
}

/// What a render needs to know about the selection.
struct Lens {
    selected: Option<NodeId>,
    /// With a goal selected: the nodes that matter to it.
    focus: Option<HashSet<NodeId>>,
}

impl Lens {
    fn in_focus(&self, id: NodeId) -> bool {
        self.focus.as_ref().is_none_or(|f| f.contains(&id))
    }
}

impl ExplView {
    /// A view of `project`, refreshed every `interval`.
    pub(crate) fn new(
        project: Project,
        interval: Duration,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let target = project.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { Snapshot::load(&target) })
                    .await;
                if this.update(cx, |view, cx| view.apply(result, cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(interval).await;
            }
        });
        Self {
            focus,
            snapshot: Snapshot::default(),
            layout: Layout::default(),
            loaded: false,
            error: None,
            selected: None,
            pan: Point::default(),
            zoom: 1.0,
            drag: None,
            pressed: None,
            reveal: false,
            epoch: Instant::now(),
            _poll: poll,
        }
    }

    fn apply(&mut self, result: Result<Snapshot, SnapshotError>, cx: &mut Context<'_, Self>) {
        match result {
            Ok(snapshot) => {
                let changed = !self.loaded || self.error.is_some() || snapshot != self.snapshot;
                if snapshot.nodes != self.snapshot.nodes
                    || snapshot.edges != self.snapshot.edges
                    || snapshot.agents != self.snapshot.agents
                {
                    let explorers: Vec<(&str, u32)> = snapshot
                        .agents
                        .iter()
                        .filter(|a| a.role == "exploration")
                        .map(|a| (a.name.as_str(), a.weight))
                        .collect();
                    self.layout =
                        Layout::of(&snapshot.nodes, &snapshot.edges, &Streams::of(&explorers));
                }
                self.snapshot = snapshot;
                self.loaded = true;
                self.error = None;
                if changed {
                    cx.notify();
                }
            }
            Err(error) => {
                let message = error.to_string();
                if self.error.as_deref() != Some(message.as_str()) {
                    self.error = Some(message);
                    cx.notify();
                }
            }
        }
    }

    fn lens(&self) -> Lens {
        let focus = self
            .selected
            .and_then(|id| self.snapshot.node(id))
            .filter(|n| n.kind == NodeKind::Goal)
            .map(|goal| self.snapshot.related(goal.id));
        Lens {
            selected: self.selected,
            focus,
        }
    }

    /// Where the graph's first column and middle row land at `zoom`,
    /// before panning: centred while the graph fits, else right-aligned so
    /// the present and the goals stay in view.
    fn anchor(&self, zoom: f32, viewport: Size<Pixels>) -> Point<Pixels> {
        let span = self.layout.width * ROW_STRIDE * zoom;
        let width = f32::from(viewport.width);
        let left = if span + 2.0 * MARGIN <= width {
            (width - span) / 2.0
        } else {
            width - RIGHT_MARGIN - span
        };
        point(px(left), px(f32::from(viewport.height) / 2.0))
    }

    /// Centre of a node's dot in window coordinates.
    fn centre(&self, id: NodeId, viewport: Size<Pixels>) -> Option<Point<Pixels>> {
        let slot = self.layout.slots.get(&id)?;
        let offset = point(
            px(slot.x * ROW_STRIDE * self.zoom),
            px(slot.row * ROW_STRIDE * self.zoom),
        );
        Some(self.anchor(self.zoom, viewport) + offset + self.pan)
    }

    /// Zoom by `factor`, keeping the graph point under `at` where it is.
    fn zoom_at(&mut self, factor: f32, at: Point<Pixels>, viewport: Size<Pixels>) {
        let zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let unscaled = (at - self.anchor(self.zoom, viewport) - self.pan) / self.zoom;
        self.zoom = zoom;
        self.pan = at - self.anchor(zoom, viewport) - unscaled * zoom;
    }

    /// Zoom and pan so the whole graph fits between the corner panels
    /// (never enlarging past 1.0).
    fn fit(&mut self, viewport: Size<Pixels>) {
        let rows = self.layout.slots.values().map(|slot| slot.row);
        let (top, bottom) = rows.fold((0.0_f32, 0.0_f32), |(lo, hi), r| (lo.min(r), hi.max(r)));
        let span_x = self.layout.width * ROW_STRIDE;
        let span_y = (bottom - top) * ROW_STRIDE;
        let room_x = (f32::from(viewport.width) - 2.0 * FIT_INSET_X).max(1.0);
        let room_y = (f32::from(viewport.height) - 2.0 * FIT_INSET_Y).max(1.0);
        let zoom = [room_x / span_x, room_y / span_y]
            .into_iter()
            .filter(|z| z.is_finite())
            .fold(1.0_f32, f32::min)
            .clamp(MIN_ZOOM, 1.0);
        let middle = point(
            px(span_x / 2.0),
            px(f32::midpoint(top, bottom) * ROW_STRIDE),
        );
        let window_centre = point(viewport.width / 2.0, viewport.height / 2.0);
        self.zoom = zoom;
        self.pan = window_centre - self.anchor(zoom, viewport) - middle * zoom;
    }

    /// Pan so `id` is in view, if it is not.
    fn reveal(&mut self, id: NodeId, viewport: Size<Pixels>) {
        let Some(at) = self.centre(id, viewport) else {
            return;
        };
        let (x, y) = (f32::from(at.x), f32::from(at.y));
        let (width, height) = (f32::from(viewport.width), f32::from(viewport.height));
        let inset = 160.0;
        if x < inset || x > width - inset || y < inset || y > height - inset {
            self.pan += point(px(width / 2.0 - x), px(height / 2.0 - y));
        }
    }

    // ------------------------------------------------------------ input

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, _: &mut Context<'_, Self>) {
        self.drag = Some(Drag {
            last: event.position,
            travelled: 0.0,
        });
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let delta = event.position - drag.last;
        drag.last = event.position;
        drag.travelled += f32::from(delta.x).abs() + f32::from(delta.y).abs();
        if drag.travelled > CLICK_SLOP {
            self.pan += delta;
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        let clicked = self.drag.take().is_some_and(|d| d.travelled <= CLICK_SLOP);
        let pressed = self.pressed.take();
        let reveal = std::mem::take(&mut self.reveal);
        if clicked {
            self.selected = match pressed {
                Some(id) if self.selected != Some(id) => Some(id),
                _ => None,
            };
            if reveal && let Some(id) = self.selected {
                self.reveal(id, window.viewport_size());
            }
            cx.notify();
        }
    }

    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Scrolling up (or two fingers pushed away) zooms in.
        let delta = event.delta.pixel_delta(window.line_height());
        let factor = (f32::from(delta.y) / 240.0).exp();
        self.zoom_at(factor, event.position, window.viewport_size());
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<'_, Self>) {
        let viewport = window.viewport_size();
        let centre = point(viewport.width / 2.0, viewport.height / 2.0);
        match event.keystroke.key.as_str() {
            "escape" => self.selected = None,
            "0" => {
                self.pan = Point::default();
                self.zoom = 1.0;
            }
            "=" | "+" => self.zoom_at(ZOOM_STEP, centre, viewport),
            "-" | "_" => self.zoom_at(1.0 / ZOOM_STEP, centre, viewport),
            "f" => self.fit(viewport),
            _ => return,
        }
        cx.notify();
    }

    /// A listener that marks `id` as pressed (selected on mouse up).
    fn press(
        id: NodeId,
        reveal: bool,
        cx: &Context<'_, Self>,
    ) -> impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static {
        cx.listener(move |this, _: &MouseDownEvent, _, _| {
            this.pressed = Some(id);
            this.reveal = reveal;
        })
    }

    // ------------------------------------------------------------ graph

    fn edges(&self, viewport: Size<Pixels>, lens: &Lens) -> Vec<Line> {
        let by_id: HashMap<NodeId, &Node> = self.snapshot.nodes.iter().map(|n| (n.id, n)).collect();
        let lineage = &self.snapshot.lineage;
        let merged = |n: &Node| n.kind == NodeKind::Step && lineage.in_trunk(n.id);
        let mut lines = Vec::with_capacity(self.snapshot.edges.len());
        for edge in &self.snapshot.edges {
            let (Some(source), Some(target)) = (by_id.get(&edge.source), by_id.get(&edge.target))
            else {
                continue;
            };
            let (Some(from), Some(to)) = (
                self.centre(source.id, viewport),
                self.centre(target.id, viewport),
            ) else {
                continue;
            };
            let selected = lens.selected == Some(source.id) || lens.selected == Some(target.id);
            let focused = lens.in_focus(source.id) && lens.in_focus(target.id);
            let mut width = 1.0;
            let mut color = match (source.kind, target.kind) {
                // The trunk itself.
                (NodeKind::Origin, NodeKind::Origin) => {
                    width = 2.0;
                    theme::alpha(theme::text(), 0.7)
                }
                // A merged branch joining the spine: as strong as the spine.
                (NodeKind::Step, NodeKind::Origin) if merged(source) => {
                    width = 2.0;
                    theme::alpha(theme::text(), if selected { 0.95 } else { 0.7 })
                }
                // Work taken into the trunk.
                (_, NodeKind::Origin) => {
                    theme::alpha(theme::text(), if selected { 0.6 } else { 0.3 })
                }
                // A subgoal and the goal it serves.
                (NodeKind::Goal, NodeKind::Goal) => {
                    theme::alpha(theme::text(), if selected { 0.6 } else { 0.25 })
                }
                (_, NodeKind::Goal) => {
                    let satisfied = self
                        .snapshot
                        .candidates
                        .get(&source.id)
                        .is_some_and(|goals| goals.contains(&target.id));
                    if satisfied {
                        theme::alpha(theme::accent(), 0.45)
                    } else if selected || (lens.focus.is_some() && focused) {
                        theme::alpha(theme::bright(), 0.18)
                    } else {
                        continue;
                    }
                }
                // Along a merged branch, from where it left the spine.
                (NodeKind::Origin | NodeKind::Step, NodeKind::Step)
                    if merged(target) && (source.kind == NodeKind::Origin || merged(source)) =>
                {
                    width = 1.5;
                    theme::alpha(theme::text(), if selected { 0.8 } else { 0.5 })
                }
                _ if selected => theme::alpha(theme::bright(), 0.4),
                _ if source.status == NodeStatus::Pruned || target.status == NodeStatus::Pruned => {
                    theme::alpha(theme::ghost(), 0.5)
                }
                _ if lens.focus.is_some() && focused => theme::alpha(theme::text(), 0.35),
                _ => Hsla::from(theme::ghost()),
            };
            if !focused {
                color = color.opacity(OUT_OF_FOCUS);
            }
            lines.push(Line {
                from: from + point(px(radius(source) + 4.0), px(0.0)),
                to: to - point(px(radius(target) + 4.0), px(0.0)),
                color,
                width,
                comet: target.status == NodeStatus::Running,
            });
        }
        // A new hypothesis and the failure it was grown from: not an edge
        // (the failure is not built on), so a faint line in the failure's
        // colour.
        for derived in &self.snapshot.nodes {
            let Some(failed) = derived.derived_from.and_then(|id| by_id.get(&id)) else {
                continue;
            };
            let (Some(from), Some(to)) = (
                self.centre(failed.id, viewport),
                self.centre(derived.id, viewport),
            ) else {
                continue;
            };
            let selected = lens.selected == Some(failed.id) || lens.selected == Some(derived.id);
            let mut color = theme::alpha(theme::failure(), if selected { 0.6 } else { 0.22 });
            if !(lens.in_focus(failed.id) && lens.in_focus(derived.id)) {
                color = color.opacity(OUT_OF_FOCUS);
            }
            lines.push(Line {
                from: from + point(px(radius(failed) + 4.0), px(0.0)),
                to: to - point(px(radius(derived) + 4.0), px(0.0)),
                color,
                width: 1.0,
                comet: false,
            });
        }
        lines
    }

    fn node(
        &self,
        node: &Node,
        centre: Point<Pixels>,
        lens: &Lens,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        let id = node.id;
        let selected = lens.selected == Some(id);
        let candidate = self.snapshot.candidates.contains_key(&id);
        let current = self.snapshot.lineage.current == Some(id);
        let label_color = match node.status {
            _ if selected || current => theme::bright(),
            NodeStatus::Running => theme::bright(),
            NodeStatus::Failed => theme::failure(),
            NodeStatus::Completed if candidate => theme::accent(),
            NodeStatus::Completed | NodeStatus::Active | NodeStatus::Superseded => theme::text(),
            NodeStatus::Proposed | NodeStatus::Planned | NodeStatus::Pruned => theme::quiet(),
        };
        let fade = if !lens.in_focus(id) {
            OUT_OF_FOCUS
        } else if !selected
            && !candidate
            && node.kind == NodeKind::Step
            && self.snapshot.left_behind(id)
        {
            BEHIND
        } else {
            1.0
        };
        let text = if node.kind == NodeKind::Origin {
            let merged = self
                .snapshot
                .lineage
                .epoch(id)
                .map_or(0, |epoch| epoch.absorbed.len());
            if merged > 0 {
                format!(
                    "{}  +{merged}  {}",
                    short(node.head_commit.as_deref()),
                    node.title
                )
            } else {
                format!("{}  {}", short(node.head_commit.as_deref()), node.title)
            }
        } else if node.investigation {
            // A question, and once answered, its answer.
            match node.answer {
                Some(answer) => format!("? {}  → {answer}", node.title),
                None => format!("? {}", node.title),
            }
        } else {
            node.title.clone()
        };
        let running = node.status == NodeStatus::Running;
        let quiet = self.zoom < QUIET_LABELS_BELOW
            && !(selected || current || running || candidate || node.kind == NodeKind::Goal);
        let label_w = (LABEL_W * self.zoom).clamp(MIN_LABEL_W, LABEL_W);
        let label = ((node.status != NodeStatus::Pruned || selected) && !quiet).then(|| {
            div()
                .max_w(px(label_w))
                .px_1()
                .bg(theme::background())
                .truncate()
                .text_size(px(11.5))
                .text_color(label_color)
                .child(text)
        });

        div()
            .id(("node", id.0 as u64))
            .absolute()
            .left(centre.x - px(label_w / 2.0))
            .top(centre.y - px(DOT_BOX / 2.0))
            .w(px(label_w))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.0))
            .opacity(fade)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, Self::press(id, false, cx))
            .child(self.dot(node, selected, candidate))
            .children(label)
            .into_any_element()
    }

    fn dot(&self, node: &Node, selected: bool, candidate: bool) -> impl IntoElement {
        let size = radius(node) * 2.0;
        let current = self.snapshot.lineage.current == Some(node.id);
        let in_trunk = self.snapshot.lineage.in_trunk(node.id);
        // Opaque, so edges passing by go behind hollow marks too.
        let base = div()
            .flex_none()
            .size(px(size))
            .rounded_full()
            .bg(theme::background());
        let mark = match (node.kind, node.status) {
            (NodeKind::Origin, _) => base.bg(if current {
                theme::bright()
            } else {
                theme::text()
            }),
            (NodeKind::Goal, NodeStatus::Pruned) => base.border_1().border_color(theme::ghost()),
            (NodeKind::Goal, _) => {
                let trunk = self
                    .snapshot
                    .trunk_verdict(node.id)
                    .is_some_and(|(origin, e)| {
                        e.satisfied && Some(origin) == self.snapshot.lineage.current
                    });
                let reached = self
                    .snapshot
                    .candidates
                    .values()
                    .any(|g| g.contains(&node.id));
                if trunk {
                    base.bg(theme::accent())
                } else {
                    base.border_1()
                        .border_color(if reached {
                            theme::accent()
                        } else {
                            theme::text()
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(reached, |el| {
                            el.child(div().size(px(6.0)).rounded_full().bg(theme::accent()))
                        })
                }
            }
            // Adopted: set into the trunk, so square.
            (NodeKind::Step, NodeStatus::Completed) if in_trunk => {
                base.rounded_xs().bg(if candidate {
                    theme::accent()
                } else {
                    theme::text()
                })
            }
            (_, NodeStatus::Proposed) => base.border_1().border_color(theme::quiet()),
            (_, NodeStatus::Planned) => base.border_1().border_color(theme::text()),
            (_, NodeStatus::Running) => base.bg(theme::bright()),
            (_, NodeStatus::Completed) => base.bg(if candidate {
                theme::accent()
            } else {
                theme::text()
            }),
            (_, NodeStatus::Failed) => base.bg(theme::failure()),
            (_, NodeStatus::Pruned | NodeStatus::Active | NodeStatus::Superseded) => {
                base.bg(theme::ghost())
            }
        };

        let id = node.id.0 as u64;
        div()
            .relative()
            .flex_none()
            .size(px(DOT_BOX))
            .flex()
            .items_center()
            .justify_center()
            .when(node.status == NodeStatus::Running, |el| {
                el.child(ripple(("ripple", id), DOT_BOX, 8.0, 26.0))
            })
            .when(current, |el| {
                el.child(ring(size + 8.0, theme::alpha(theme::text(), 0.6)))
            })
            // A merge: this origin took work into the trunk.
            .when(!current && self.merges(node), |el| {
                el.child(ring(size + 6.0, theme::alpha(theme::text(), 0.45)))
            })
            .when(selected, |el| {
                el.child(ring(
                    size + if current { 16.0 } else { 8.0 },
                    theme::alpha(theme::bright(), 0.5),
                ))
            })
            .child(mark)
    }

    /// Whether `node` is an origin that took steps into the trunk.
    fn merges(&self, node: &Node) -> bool {
        node.kind == NodeKind::Origin
            && self
                .snapshot
                .lineage
                .epoch(node.id)
                .is_some_and(|epoch| !epoch.adopted.is_empty())
    }

    // ------------------------------------------------------------ corners

    /// The selected node, in words.
    fn detail(&self) -> Option<impl IntoElement> {
        let node = self.selected.and_then(|id| self.snapshot.node(id))?;
        let lineage = &self.snapshot.lineage;
        let working = self.snapshot.working_on();
        let worker = working
            .get(&node.id)
            .and_then(|runs| runs.first())
            .map(|run| {
                run.process
                    .and_then(|p| self.snapshot.processes.iter().find(|r| r.id.0 == p))
                    .map_or_else(|| run.agent.clone(), |r| r.name.clone())
            });

        let mut header = vec![node.id.to_string()];
        let mut context: Option<String> = None;
        let mut rows: Vec<Row> = Vec::new();
        match node.kind {
            NodeKind::Origin => {
                header.push(if lineage.current == Some(node.id) {
                    "current".to_owned()
                } else {
                    let behind = lineage.behind(node.id).unwrap_or(0);
                    format!("{behind} behind")
                });
                header.push(format!(
                    "{} @ {}",
                    node.branch.as_deref().unwrap_or("-"),
                    short(node.head_commit.as_deref())
                ));
                if let Some(epoch) = lineage.epoch(node.id) {
                    context = Some(self.epoch_summary(epoch));
                    rows.extend(epoch.absorbed.iter().rev().filter_map(|step| {
                        let title = &self.snapshot.node(*step)?.title;
                        Some(Row::plain(step.to_string(), title.clone()))
                    }));
                }
                rows.extend(self.verdict_rows(node.id));
            }
            NodeKind::Goal => {
                header.push(if node.status == NodeStatus::Active {
                    "active".to_owned()
                } else {
                    "retired".to_owned()
                });
                if node.counter_exploration {
                    header.push("counter".to_owned());
                }
                context = self.snapshot.trunk_verdict(node.id).map(|(origin, e)| {
                    format!(
                        "trunk {} at {origin}{}",
                        e.score,
                        if e.satisfied { ", satisfied" } else { "" }
                    )
                });
                let mut candidates: Vec<Row> = self
                    .snapshot
                    .candidates
                    .iter()
                    .filter(|(_, goals)| goals.contains(&node.id))
                    .filter_map(|(step, _)| {
                        let title = &self.snapshot.node(*step)?.title;
                        let score = self
                            .snapshot
                            .verdicts(*step)
                            .into_iter()
                            .find(|e| e.goal == node.id)
                            .map_or(0, |e| e.score);
                        let adopted = lineage
                            .absorbed_by(*step)
                            .map(|o| format!("  · in trunk {o}"))
                            .unwrap_or_default();
                        Some(Row::scored(
                            score,
                            true,
                            format!("{step}  {title}{adopted}"),
                        ))
                    })
                    .collect();
                candidates.sort_by_key(|r| std::cmp::Reverse(r.score));
                rows.extend(candidates);
            }
            NodeKind::Step => {
                if node.investigation {
                    header.push("investigation".to_owned());
                }
                header.push(match node.answer {
                    Some(answer) => format!("{} — answered {answer}", node.status),
                    None => node.status.to_string(),
                });
                if let Some(worker) = worker {
                    header.push(worker);
                }
                if let Some(failed) = node.derived_from {
                    header.push(format!("grown from the failure of {failed}"));
                }
                let grown: Vec<String> = self
                    .snapshot
                    .nodes
                    .iter()
                    .filter(|n| n.derived_from == Some(node.id))
                    .map(|n| n.id.to_string())
                    .collect();
                if !grown.is_empty() {
                    header.push(format!("grew {}", grown.join(" ")));
                }
                context = match (lineage.absorbed_by(node.id), lineage.explored_from(node.id)) {
                    (Some(origin), _) => Some(format!("in the trunk since {origin}")),
                    (None, Some(from)) => match lineage.behind(from) {
                        Some(0) | None => Some(format!("from {from}, the current origin")),
                        Some(n) => Some(format!(
                            "from {from}, {n} origin{} behind",
                            if n == 1 { "" } else { "s" }
                        )),
                    },
                    (None, None) => None,
                };
                rows.extend(self.verdict_rows(node.id));
            }
        }
        let branch = (node.kind == NodeKind::Step && node.status == NodeStatus::Completed)
            .then(|| node.branch.clone())
            .flatten();
        let remark = match node.kind {
            NodeKind::Origin => Some(node.objective.clone()),
            NodeKind::Step | NodeKind::Goal => node.outcome.clone(),
        }
        .filter(|o| !o.trim().is_empty());

        Some(
            div()
                .absolute()
                .top(px(56.0))
                .left(px(32.0))
                .w(px(DETAIL_W))
                .flex()
                .flex_col()
                .gap_2()
                .child(caption(header.join("  ·  ")))
                .child(
                    div()
                        .text_size(px(17.0))
                        .text_color(theme::bright())
                        .line_clamp(2)
                        .child(node.title.clone()),
                )
                .when(
                    node.kind != NodeKind::Origin && !node.objective.is_empty(),
                    |el| {
                        el.child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme::text())
                                .line_clamp(4)
                                .child(node.objective.clone()),
                        )
                    },
                )
                .when_some(context, |el, context| {
                    el.child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::quiet())
                            .child(context),
                    )
                })
                .when(!rows.is_empty(), |el| {
                    el.child(div().h(px(4.0)))
                        .children(rows.into_iter().map(Row::render))
                })
                .when_some(remark, |el, remark| {
                    el.child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::quiet())
                            .line_clamp(3)
                            .child(remark),
                    )
                })
                .when_some(branch, |el, branch| el.child(caption(branch))),
        )
    }

    /// The latest verdicts of `node`, best first.
    fn verdict_rows(&self, node: NodeId) -> Vec<Row> {
        self.snapshot
            .verdicts(node)
            .into_iter()
            .map(|e| {
                let title = self
                    .snapshot
                    .node(e.goal)
                    .map_or_else(|| e.goal.to_string(), |g| g.title.clone());
                Row::scored(e.score, e.satisfied, title)
            })
            .collect()
    }

    /// One line on what an origin brought into the trunk.
    fn epoch_summary(&self, epoch: &Epoch) -> String {
        let title = |id: NodeId| {
            self.snapshot
                .node(id)
                .map_or_else(|| id.to_string(), |n| format!("{id} {}", n.title))
        };
        match (epoch.previous, epoch.adopted.as_slice()) {
            (None, _) => "where the exploration began".to_owned(),
            (Some(_), []) => "the trunk moved by other work".to_owned(),
            (Some(_), [one]) => format!("adopted {}", title(*one)),
            (Some(_), [first, rest @ ..]) => {
                format!("adopted {} and {} more", title(*first), rest.len())
            }
        }
    }

    /// Goals top right: where the trunk stands on each, and the best
    /// candidate still open. A goal can be selected to focus the graph.
    fn goals_panel(&self, lens: &Lens, cx: &Context<'_, Self>) -> impl IntoElement {
        let goals = self.snapshot.goals();
        div()
            .absolute()
            .top(px(56.0))
            .right(px(32.0))
            .w(px(GOALS_W))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .when(!goals.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .gap_3()
                        .child(div().flex_1().child(caption("GOALS")))
                        .child(div().w(px(32.0)).child(caption("TRUNK")))
                        .child(div().w(px(32.0)).child(caption("BEST"))),
                )
            })
            .children(goals.into_iter().map(|goal| {
                let active = goal.status == NodeStatus::Active;
                // Subgoals sit indented under the goals they serve.
                let indent = small_f32(self.snapshot.goal_tree.depth(goal.id)) * SUBGOAL_INDENT;
                let selected = lens.selected == Some(goal.id);
                let trunk = self.snapshot.trunk_verdict(goal.id).map(|(_, e)| e);
                let best = self.snapshot.best_open(goal.id);
                let reached = best.is_some();
                let marker = div()
                    .flex_none()
                    .size(px(8.0))
                    .rounded_full()
                    .map(|el| match trunk {
                        Some(e) if e.satisfied && active => el.bg(theme::accent()),
                        _ => el.border_1().border_color(if !active {
                            theme::ghost()
                        } else if reached {
                            theme::accent()
                        } else {
                            theme::text()
                        }),
                    });
                let title_color = if selected {
                    theme::bright()
                } else if active {
                    theme::text()
                } else {
                    theme::quiet()
                };
                div()
                    .id(("goal", goal.id.0 as u64))
                    .flex()
                    .items_center()
                    .gap_3()
                    .pl(px(indent))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, Self::press(goal.id, true, cx))
                    .child(marker)
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(title_color)
                            .when(!active, Styled::line_through)
                            .child(goal.title.clone()),
                    )
                    .child(score_cell(
                        trunk.map(|e| e.score),
                        trunk.is_some_and(|e| e.satisfied),
                        theme::text(),
                    ))
                    .child(score_cell(
                        best.map(|e| e.score),
                        best.is_some_and(|e| e.satisfied),
                        theme::accent(),
                    ))
            }))
    }

    /// The trunk's history bottom right, newest first: what was chosen.
    fn trunk_panel(&self, lens: &Lens, cx: &Context<'_, Self>) -> impl IntoElement {
        let epochs = &self.snapshot.lineage.epochs;
        let earlier = epochs.len().saturating_sub(TRUNK_ROWS);
        div()
            .absolute()
            .bottom(px(28.0))
            .right(px(32.0))
            .w(px(TRUNK_W))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .when(!epochs.is_empty(), |el| el.child(caption("TRUNK")))
            .children(epochs.iter().rev().take(TRUNK_ROWS).map(|epoch| {
                let current = self.snapshot.lineage.current == Some(epoch.origin);
                let selected = lens.selected == Some(epoch.origin);
                let commit = self
                    .snapshot
                    .node(epoch.origin)
                    .map(|n| short(n.head_commit.as_deref()))
                    .unwrap_or_default();
                let color: Rgba = if selected || current {
                    theme::bright()
                } else {
                    theme::text()
                };
                div()
                    .id(("trunk", epoch.origin.0 as u64))
                    .flex()
                    .items_center()
                    .gap_3()
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, Self::press(epoch.origin, true, cx))
                    .child(
                        div()
                            .flex_none()
                            .size(px(6.0))
                            .rounded_full()
                            .bg(if current {
                                theme::bright()
                            } else {
                                theme::quiet()
                            }),
                    )
                    .child(
                        div()
                            .w(px(84.0))
                            .flex_none()
                            .font_family(MONO)
                            .text_size(px(10.5))
                            .text_color(theme::quiet())
                            .child(format!("{} {commit}", epoch.origin)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(color)
                            .child(self.epoch_summary(epoch)),
                    )
            }))
            .when(earlier > 0, |el| {
                el.child(
                    div()
                        .pl(px(18.0))
                        .text_size(px(11.0))
                        .text_color(theme::quiet())
                        .child(format!("+{earlier} earlier")),
                )
            })
    }

    /// One dot per process, grouped by agent.
    fn agents(&self) -> impl IntoElement {
        let busy = self.snapshot.busy();
        let mut agents: Vec<(&str, usize)> = self
            .snapshot
            .agents
            .iter()
            .map(|a| (a.name.as_str(), a.replicas))
            .collect();
        for process in &self.snapshot.processes {
            if !agents.iter().any(|(name, _)| *name == process.agent) {
                agents.push((process.agent.as_str(), 0));
            }
        }
        div()
            .absolute()
            .bottom(px(28.0))
            .left(px(32.0))
            .flex()
            .items_center()
            .gap(px(14.0))
            .children(agents.into_iter().map(|(name, replicas)| {
                let processes: Vec<&ProcessRecord> = self.snapshot.processes_of(name).collect();
                let missing = replicas.saturating_sub(processes.len());
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .children(
                        processes
                            .into_iter()
                            .map(|p| process_dot(p, busy.contains_key(&p.id))),
                    )
                    .children((0..missing).map(|_| {
                        div()
                            .size(px(6.0))
                            .rounded_full()
                            .border_1()
                            .border_color(theme::ghost())
                    }))
            }))
    }
}

impl Render for ExplView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let lens = self.lens();
        let lines = self.edges(viewport, &lens);
        let phase = (self.epoch.elapsed().as_secs_f32() / COMET.as_secs_f32()).fract();
        let nodes: Vec<AnyElement> = self
            .snapshot
            .nodes
            .iter()
            .filter_map(|node| Some(self.node(node, self.centre(node.id, viewport)?, &lens, cx)))
            .collect();
        let empty = self.loaded && !self.snapshot.nodes.iter().any(|n| n.kind == NodeKind::Goal);

        div()
            .id("expl")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(theme::background())
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, (), window, _| {
                        let mut moving = false;
                        for line in &lines {
                            line.paint(bounds.origin, window);
                            if line.comet {
                                moving = true;
                                line.paint_comet(bounds.origin, phase, window);
                            }
                        }
                        if moving {
                            window.request_animation_frame();
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .children(nodes)
            .when(empty, |el| {
                el.child(
                    div()
                        .absolute()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.0))
                        .text_color(theme::quiet())
                        .child("no goals"),
                )
            })
            .children(self.detail())
            .child(self.goals_panel(&lens, cx))
            .child(self.trunk_panel(&lens, cx))
            .child(self.agents())
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .absolute()
                        .top(px(18.0))
                        .left_0()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .max_w(px(560.0))
                                .truncate()
                                .text_size(px(11.0))
                                .text_color(theme::failure())
                                .child(error),
                        ),
                )
            })
    }
}

// ---------------------------------------------------------------- pieces

/// Dot radius of a node.
fn radius(node: &Node) -> f32 {
    match (node.kind, node.status) {
        (NodeKind::Goal, _) => 7.0,
        (NodeKind::Origin, NodeStatus::Active) => 6.0,
        (NodeKind::Origin, _) => 5.0,
        (_, NodeStatus::Pruned) => 2.0,
        _ => 4.0,
    }
}

/// The first 7 characters of a commit id.
fn short(commit: Option<&str>) -> String {
    commit.map_or_else(|| "-".to_owned(), |c| c.chars().take(7).collect())
}

/// Small monospace label.
fn caption(text: impl Into<gpui::SharedString>) -> impl IntoElement {
    div()
        .font_family(MONO)
        .text_size(px(10.5))
        .text_color(theme::quiet())
        .child(text.into())
}

/// A score in a goals-panel column; `-` when there is none.
fn score_cell(score: Option<i64>, satisfied: bool, satisfied_color: Rgba) -> impl IntoElement {
    div()
        .w(px(32.0))
        .flex_none()
        .font_family(MONO)
        .text_size(px(11.0))
        .text_color(match score {
            Some(_) if satisfied => satisfied_color,
            Some(_) => theme::text(),
            None => theme::ghost(),
        })
        .child(score.map_or_else(|| "-".to_owned(), |s| s.to_string()))
}

/// A circle outline of `diameter`, centred in the dot box.
fn ring(diameter: f32, color: Hsla) -> impl IntoElement {
    div()
        .absolute()
        .size(px(diameter))
        .left(px((DOT_BOX - diameter) / 2.0))
        .top(px((DOT_BOX - diameter) / 2.0))
        .rounded_full()
        .border_1()
        .border_color(color)
}

/// One line of the detail panel: an optional score, then words.
struct Row {
    score: Option<i64>,
    satisfied: bool,
    label: String,
    text: String,
}

impl Row {
    fn scored(score: i64, satisfied: bool, text: String) -> Self {
        Self {
            score: Some(score),
            satisfied,
            label: score.to_string(),
            text,
        }
    }

    fn plain(label: String, text: String) -> Self {
        Self {
            score: None,
            satisfied: false,
            label,
            text,
        }
    }

    fn render(self) -> impl IntoElement {
        let color = match self.score {
            Some(_) if self.satisfied => theme::accent(),
            Some(_) => theme::text(),
            None => theme::quiet(),
        };
        div()
            .flex()
            .gap_3()
            .text_size(px(12.0))
            .child(
                div()
                    .w(px(28.0))
                    .flex_none()
                    .font_family(MONO)
                    .text_color(color)
                    .child(self.label),
            )
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_color(theme::text())
                    .child(self.text),
            )
    }
}

/// A ring of light expanding from the centre of a `square`-sized box and
/// fading out: the mark of work in progress, on nodes and agents alike.
fn ripple(id: impl Into<ElementId>, square: f32, from: f32, to: f32) -> impl IntoElement {
    div()
        .absolute()
        .rounded_full()
        .bg(theme::bright())
        .with_animation(
            id,
            Animation::new(Duration::from_millis(1600)).repeat(),
            move |el, delta| {
                let size = from + (to - from) * delta;
                let offset = (square - size) / 2.0;
                el.size(px(size))
                    .left(px(offset))
                    .top(px(offset))
                    .opacity(0.4 * (1.0 - delta))
            },
        )
}

fn process_dot(process: &ProcessRecord, busy: bool) -> AnyElement {
    let dot = div().size(px(6.0)).rounded_full();
    match process.state {
        ProcessState::Running if busy => div()
            .relative()
            .size(px(6.0))
            .child(ripple(("process", process.id.0 as u64), 6.0, 6.0, 20.0))
            .child(dot.bg(theme::bright()))
            .into_any_element(),
        ProcessState::Running => dot.bg(theme::quiet()).into_any_element(),
        ProcessState::Created => dot
            .border_1()
            .border_color(theme::quiet())
            .into_any_element(),
        ProcessState::Dead => dot.bg(theme::failure()).into_any_element(),
        ProcessState::Exited => dot.bg(theme::ghost()).into_any_element(),
    }
}

/// One edge, ready to paint.
struct Line {
    from: Point<Pixels>,
    to: Point<Pixels>,
    color: Hsla,
    width: f32,
    comet: bool,
}

impl Line {
    /// Control points of the horizontal S-curve.
    fn controls(&self) -> (Point<Pixels>, Point<Pixels>) {
        let reach = ((self.to.x - self.from.x) * 0.5).max(px(24.0));
        (
            point(self.from.x + reach, self.from.y),
            point(self.to.x - reach, self.to.y),
        )
    }

    fn paint(&self, offset: Point<Pixels>, window: &mut Window) {
        let (first, second) = self.controls();
        let mut path = PathBuilder::stroke(px(self.width));
        path.move_to(offset + self.from);
        path.cubic_bezier_to(offset + self.to, offset + first, offset + second);
        if let Ok(path) = path.build() {
            window.paint_path(path, self.color);
        }
    }

    /// A short bright segment travelling along the edge.
    fn paint_comet(&self, offset: Point<Pixels>, phase: f32, window: &mut Window) {
        let (first, second) = self.controls();
        let at = |progress: f32| offset + cubic(self.from, first, second, self.to, progress);
        let tail = (phase - 0.18).max(0.0);
        let mut path = PathBuilder::stroke(px(1.5));
        path.move_to(at(tail));
        for step in 1..=8u8 {
            path.line_to(at(tail + (phase - tail) * f32::from(step) / 8.0));
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, theme::alpha(theme::bright(), 0.8));
        }
    }
}

/// The point at `progress` (0–1) along a cubic Bézier.
fn cubic(
    start: Point<Pixels>,
    first: Point<Pixels>,
    second: Point<Pixels>,
    end: Point<Pixels>,
    progress: f32,
) -> Point<Pixels> {
    let rest = 1.0 - progress;
    let weights = [
        rest * rest * rest,
        3.0 * rest * rest * progress,
        3.0 * rest * progress * progress,
        progress * progress * progress,
    ];
    let points = [start, first, second, end];
    let mix = |axis: fn(&Point<Pixels>) -> Pixels| -> Pixels {
        px(points
            .iter()
            .zip(weights)
            .map(|(p, w)| f32::from(axis(p)) * w)
            .sum())
    };
    point(mix(|p| p.x), mix(|p| p.y))
}

/// Small counts as `f32`; saturating at `u16::MAX` is harmless.
fn small_f32(n: usize) -> f32 {
    f32::from(u16::try_from(n).unwrap_or(u16::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_starts_and_ends_at_its_endpoints() {
        let start = point(px(0.0), px(0.0));
        let end = point(px(100.0), px(40.0));
        let first = point(px(50.0), px(0.0));
        let second = point(px(50.0), px(40.0));
        assert_eq!(cubic(start, first, second, end, 0.0), start);
        assert_eq!(cubic(start, first, second, end, 1.0), end);
        let middle = cubic(start, first, second, end, 0.5);
        assert!((f32::from(middle.x) - 50.0).abs() < 0.01);
        assert!((f32::from(middle.y) - 20.0).abs() < 0.01);
    }
}
