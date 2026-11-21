//! Exploration — identify nodes that could lead toward the goals.
//!
//! - `goal.added` → explore forward from the origin toward the new goal;
//!   when the goal asks for counter-exploration, also explore backward
//!   from it: what must hold just before it is reached.
//! - `node.evaluated` → explore forward from the evaluated step: what
//!   could come next, given how far it got.
//! - `origin.advanced` → the trunk moved: explore forward from the new
//!   origin toward every active goal, and backward from each goal that
//!   asks for counter-exploration, as for a new goal.
//! - `node.revisited` → an idle exploration process picked a starting
//!   point at random ([`Graph::starting_points`]): explore forward from it,
//!   or backward when it is a counter-exploration goal. One exploration
//!   never exhausts a starting point, so the same node is explored again
//!   and again, each time growing new steps.
//!
//! - `node.failed` → a step could not be carried out: explore past the
//!   failure — new hypotheses that build on what the failed step built on
//!   and head for its goals, informed by what went wrong. A failed step is
//!   also a starting point that revisits come back to.
//!
//! An agent declared with `speculate = true` also counts steps not
//! carried out yet among its starting points: revisiting one explores
//! ahead of it — assuming it and the open steps it builds on succeed — so
//! the map grows deep before anything runs. Only revisits explore ahead, so
//! the revisit cooldown bounds how fast that happens.
//!
//! An agent declared with `scope = "graph"` reacts to the same events
//! differently: it surveys the whole graph at once — every step with its
//! outcome and evaluations — for what no single node would suggest (gaps
//! between paths, combinations of separate paths, patterns across
//! failures), and decides itself where each proposal attaches. A survey
//! covers every event recorded before it started, so events already older
//! than the agent's latest survey are passed over.
//!
//! Every exploration is told which steps already exist around it (the
//! ones built on its starting point, and all open work), so a repeated
//! exploration proposes alternatives instead of the same steps again.
//!
//! The agent works read-only in a scratch worktree at the node it explores
//! from (the origin, for backward exploration), so its proposals are
//! grounded in the actual code. Its prompt holds only the goals it explores
//! toward; where proposals attach in the graph is decided here, not by the
//! agent — except in a survey, whose point is to attach anywhere.

use std::collections::BTreeMap;
use std::fmt::Write;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::declaration::Scope;
use crate::event::EventKind;
use crate::graph::{Evaluation, Graph, Lineage, Node, NodeFilter, NodeId, NodeKind, NodeStatus};
use crate::prompt::Prompt;
use crate::role::{Direction, Reaction, Report, RoleError, digest};

/// What one exploration did.
#[derive(Debug, Clone, Serialize)]
pub struct ExplorationReport {
    /// Search direction.
    pub direction: Direction,
    /// The node explored from.
    pub from: NodeId,
    /// Steps added to the graph.
    pub proposed: Vec<NodeId>,
    /// Proposals the graph refused.
    pub rejected: Vec<Rejection>,
}

/// A proposal the graph refused, and why.
#[derive(Debug, Clone, Serialize)]
pub struct Rejection {
    /// Proposed title.
    pub title: String,
    /// Why it was refused.
    pub reason: String,
}

#[derive(Debug, Deserialize)]
struct Response {
    nodes: Vec<ProposedNode>,
}

#[derive(Debug, Deserialize)]
struct ProposedNode {
    title: String,
    objective: String,
    /// The goals it heads for (forward exploration only).
    #[serde(default)]
    before: Vec<i64>,
}

#[derive(Debug, Deserialize)]
struct SurveyResponse {
    nodes: Vec<SurveyedNode>,
}

#[derive(Debug, Deserialize)]
struct SurveyedNode {
    title: String,
    objective: String,
    after: Vec<i64>,
    before: Vec<i64>,
}

fn survey_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["nodes"],
        "properties": {
            "nodes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["title", "objective", "after", "before"],
                    "properties": {
                        "title": {"type": "string", "description": "Short imperative name of the step."},
                        "objective": {"type": "string", "description": "What the step must achieve, concretely enough to be done in one session and checked afterwards."},
                        "after": {"type": "array", "items": {"type": "integer"}, "description": "Ids of the nodes it builds on: the origin, or completed steps. Several ids join their work; empty means the origin."},
                        "before": {"type": "array", "items": {"type": "integer"}, "description": "Ids of the goals it heads toward — a subgoal, a goal directly, or several; empty for the default named in the task."}
                    }
                }
            }
        }
    })
}

/// The response schema; with `aiming`, each proposal names the goals it
/// heads for.
fn schema(aiming: bool) -> serde_json::Value {
    let mut item = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["title", "objective"],
        "properties": {
            "title": {"type": "string", "description": "Short imperative name of the step."},
            "objective": {"type": "string", "description": "What the step must achieve, concretely enough to be done in one session and checked afterwards."}
        }
    });
    if aiming {
        item["required"] = json!(["title", "objective", "before"]);
        item["properties"]["before"] = json!({
            "type": "array",
            "items": {"type": "integer"},
            "description": "Ids of the goals the step heads toward; empty for the default named in the task."
        });
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["nodes"],
        "properties": {
            "nodes": {"type": "array", "items": item}
        }
    })
}

/// The goals an exploration heads for, and the ones a proposal that names
/// none of them heads for.
struct Aim {
    goals: Vec<Node>,
    default: Vec<NodeId>,
}

impl Aim {
    /// Just `goal` (backward exploration).
    fn single(goal: &Node) -> Self {
        Self {
            goals: vec![goal.clone()],
            default: vec![goal.id],
        }
    }

    /// A goal just added, with the goals it serves for context; proposals
    /// head for it unless they say otherwise.
    fn at(graph: &Graph, goal: &Node) -> Result<Self, RoleError> {
        let ancestors = graph.goal_tree()?.ancestors(goal.id);
        let mut goals = vec![goal.clone()];
        goals.extend(
            graph
                .goals()?
                .into_iter()
                .filter(|g| ancestors.contains(&g.id)),
        );
        Ok(Self {
            goals,
            default: vec![goal.id],
        })
    }

    /// Every active goal; proposals that name none head for the top-level
    /// goals, tied to no subgoal.
    fn everywhere(graph: &Graph) -> Result<Self, RoleError> {
        let goals = graph.goals()?;
        let ids: Vec<NodeId> = goals.iter().map(|g| g.id).collect();
        let default = graph.goal_tree()?.roots(&ids);
        Ok(Self { goals, default })
    }

    /// The active goals `start` leads to (a subgoal brings the goals it
    /// serves); proposals that name none head for the most specific of
    /// them. From the origin, or a step that leads to no active goal:
    /// [`everywhere`](Self::everywhere).
    fn from(graph: &Graph, start: &Node) -> Result<Self, RoleError> {
        if start.kind == NodeKind::Origin {
            return Self::everywhere(graph);
        }
        let goals: Vec<Node> = graph
            .goals_reachable_from(start.id)?
            .into_iter()
            .filter(|goal| goal.status == NodeStatus::Active)
            .collect();
        if goals.is_empty() {
            return Self::everywhere(graph);
        }
        let ids: Vec<NodeId> = goals.iter().map(|g| g.id).collect();
        let default = graph.goal_tree()?.leaves(&ids);
        Ok(Self { goals, default })
    }

    fn is_empty(&self) -> bool {
        self.goals.is_empty()
    }

    /// The goals a proposal naming `named` heads for: those of its aim it
    /// names, else the default.
    fn resolve(&self, named: &[i64]) -> Vec<NodeId> {
        let mut before: Vec<NodeId> = Vec::new();
        for id in named.iter().map(|id| NodeId(*id)) {
            if self.goals.iter().any(|g| g.id == id) && !before.contains(&id) {
                before.push(id);
            }
        }
        if before.is_empty() {
            before.clone_from(&self.default);
        }
        before
    }

    /// The goals as a forward task lists them, and how a proposal says
    /// which it heads for.
    fn describe(&self, graph: &Graph) -> Result<String, RoleError> {
        let tree = graph.goal_tree()?;
        let mut out = digest::numbered_goals(&self.goals, &tree);
        if self.goals.len() > 1 {
            let default: Vec<String> = self
                .default
                .iter()
                .map(|id| format!("[{}]", id.0))
                .collect();
            let _ = write!(
                out,
                "\nFor each step, list in `before` the ids of the goals it heads toward — a \
                 subgoal, a goal directly, or several. Leave it empty to head for {}.\n",
                default.join(", ")
            );
        }
        Ok(out)
    }
}

pub(crate) fn react(reaction: &Reaction<'_>, graph: &mut Graph) -> Result<Report, RoleError> {
    let subject = graph.node(reaction.subject()?)?;
    if reaction.agent().declaration().scope == Some(Scope::Graph) {
        return survey_on(reaction, graph, &subject);
    }
    let explorations = match reaction.event().kind {
        EventKind::GoalAdded => {
            if subject.status != NodeStatus::Active {
                return Ok(Reaction::ignore(format!(
                    "goal {} is {}",
                    subject.id, subject.status
                )));
            }
            let origin = graph.origin()?;
            let aim = Aim::at(graph, &subject)?;
            let mut explorations =
                vec![explore(reaction, graph, Direction::Forward, &origin, &aim)?];
            if subject.counter_exploration {
                explorations.push(explore(
                    reaction,
                    graph,
                    Direction::Backward,
                    &subject,
                    &Aim::single(&subject),
                )?);
            }
            explorations
        }
        EventKind::OriginAdvanced => {
            if subject.kind != NodeKind::Origin || subject.status != NodeStatus::Active {
                return Ok(Reaction::ignore(format!(
                    "{} is a {} {}; the trunk has moved on",
                    subject.id, subject.status, subject.kind
                )));
            }
            let aim = Aim::everywhere(graph)?;
            if aim.is_empty() {
                return Ok(Reaction::ignore("no active goals"));
            }
            let mut explorations = vec![explore(
                reaction,
                graph,
                Direction::Forward,
                &subject,
                &aim,
            )?];
            for goal in aim.goals.iter().filter(|g| g.counter_exploration) {
                explorations.push(explore(
                    reaction,
                    graph,
                    Direction::Backward,
                    goal,
                    &Aim::single(goal),
                )?);
            }
            explorations
        }
        EventKind::NodeRevisited => {
            let ahead = reaction.agent().declaration().speculate == Some(true);
            if !graph
                .starting_points(ahead)?
                .iter()
                .any(|n| n.id == subject.id)
            {
                return Ok(Reaction::ignore(format!(
                    "{} ({} {}) is no longer a starting point",
                    subject.id, subject.status, subject.kind
                )));
            }
            if matches!(subject.status, NodeStatus::Proposed | NodeStatus::Planned) {
                match explore_ahead(reaction, graph, &subject)? {
                    Some(report) => vec![report],
                    None => {
                        return Ok(Reaction::ignore(format!(
                            "{} builds on a failed or pruned step",
                            subject.id
                        )));
                    }
                }
            } else if subject.status == NodeStatus::Failed {
                vec![explore_failure(reaction, graph, &subject)?]
            } else if subject.kind == NodeKind::Goal {
                vec![explore(
                    reaction,
                    graph,
                    Direction::Backward,
                    &subject,
                    &Aim::single(&subject),
                )?]
            } else {
                let aim = Aim::from(graph, &subject)?;
                vec![explore(
                    reaction,
                    graph,
                    Direction::Forward,
                    &subject,
                    &aim,
                )?]
            }
        }
        EventKind::NodeEvaluated => {
            if subject.kind != NodeKind::Step || subject.status != NodeStatus::Completed {
                return Ok(Reaction::ignore(format!(
                    "{} is a {} {}",
                    subject.id, subject.status, subject.kind
                )));
            }
            let aim = Aim::from(graph, &subject)?;
            if aim.is_empty() {
                return Ok(Reaction::ignore("no active goals"));
            }
            vec![explore(
                reaction,
                graph,
                Direction::Forward,
                &subject,
                &aim,
            )?]
        }
        EventKind::NodeFailed => {
            if subject.kind != NodeKind::Step || subject.status != NodeStatus::Failed {
                return Ok(Reaction::ignore(format!(
                    "{} is a {} {}",
                    subject.id, subject.status, subject.kind
                )));
            }
            if graph.goals()?.is_empty() {
                return Ok(Reaction::ignore("no active goals"));
            }
            vec![explore_failure(reaction, graph, &subject)?]
        }
        other @ (EventKind::GoalRetired
        | EventKind::NodeProposed
        | EventKind::NodePlanned
        | EventKind::NodeReady
        | EventKind::NodeStarted
        | EventKind::NodeCompleted
        | EventKind::NodePruned
        | EventKind::PlanRecorded) => {
            return Ok(Reaction::ignore(format!(
                "exploration does not react to {other}"
            )));
        }
    };
    Ok(Report::Exploration { explorations })
}

/// Most steps an exploration is told already exist.
const KNOWN_STEPS: usize = 40;

/// Longest excerpt of a failed step's outcome quoted to an exploration
/// past it.
const FAILURE_EXCERPT: usize = 2000;

/// The steps that already exist around an exploration: `around` (the
/// neighbours of where it starts), whatever became of them, and every step
/// still open anywhere — newest first.
fn known_steps(graph: &Graph, mut steps: Vec<Node>) -> Result<Vec<Node>, RoleError> {
    steps.extend(graph.nodes(&NodeFilter {
        kinds: vec![NodeKind::Step],
        statuses: vec![
            NodeStatus::Proposed,
            NodeStatus::Planned,
            NodeStatus::Running,
        ],
    })?);
    steps.retain(|n| n.kind == NodeKind::Step);
    steps.sort_by_key(|n| std::cmp::Reverse(n.id));
    steps.dedup_by_key(|n| n.id);
    steps.truncate(KNOWN_STEPS);
    Ok(steps)
}

/// Where new work on top of `nodes` can start: completed steps not yet in
/// the trunk stay themselves; origins and steps already in the trunk
/// become the active `origin`; anything else (open, failed, pruned work,
/// goals) is dropped. The origin is dropped next to a step, which already
/// builds on an origin. Never empty: the origin when nothing is left.
fn realised_base(lineage: &Lineage, origin: &Node, nodes: &[&Node]) -> Vec<NodeId> {
    let mut base = Vec::new();
    for node in nodes {
        let id = match (node.kind, node.status) {
            (NodeKind::Step, NodeStatus::Completed) if !lineage.in_trunk(node.id) => node.id,
            (NodeKind::Origin, _) | (NodeKind::Step, NodeStatus::Completed) => origin.id,
            _ => continue,
        };
        if !base.contains(&id) {
            base.push(id);
        }
    }
    if base.len() > 1 {
        base.retain(|id| *id != origin.id);
    }
    if base.is_empty() {
        base.push(origin.id);
    }
    base
}

/// Add one proposal to the graph, noting it in `report`.
fn record(
    graph: &mut Graph,
    report: &mut ExplorationReport,
    title: &str,
    objective: &str,
    after: &[NodeId],
    before: &[NodeId],
) {
    match graph.propose_step(title.trim(), objective.trim(), after, before) {
        Ok(id) => report.proposed.push(id),
        Err(error) => report.rejected.push(Rejection {
            title: title.to_owned(),
            reason: error.to_string(),
        }),
    }
}

fn breadth(reaction: &Reaction<'_>) -> usize {
    reaction.agent().declaration().breadth.unwrap_or(1)
}

/// Explore from `start` toward `goals`, forward or backward. Forward
/// proposals build on `start` and lead to `goals`; backward proposals
/// (`start` is the goal) build on the origin and lead to `start`.
fn explore(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    direction: Direction,
    start: &Node,
    aim: &Aim,
) -> Result<ExplorationReport, RoleError> {
    let origin = graph.origin()?;
    let backward = direction == Direction::Backward;
    let (ground, after): (&Node, Vec<NodeId>) = if backward {
        (&origin, vec![origin.id])
    } else {
        (start, vec![start.id])
    };
    let breadth = breadth(reaction);
    let known = if backward {
        known_steps(graph, graph.predecessors(start.id)?)?
    } else {
        known_steps(graph, graph.successors(start.id)?)?
    };
    let task = if backward {
        format!(
            "## Task\n\n\
             The target below is a state this repository should reach. Work backward from it: \
             propose up to {breadth} possible steps that could be realised immediately before \
             the target is reached. The working directory holds the repository as it is now; read it to \
             ground your proposals. Do not modify files.\n\n\
             ## Target\n\n{target}",
            target = digest::goals(std::slice::from_ref(start)),
        )
    } else {
        format!(
            "## Task\n\n\
             Propose up to {breadth} possible next steps that could move this repository toward \
             the goal{s} below. The working directory holds the repository as it is now; read it to ground \
             your proposals. Do not modify files.\n\n\
             ## Goal{s}\n\n{goals}{judged}",
            s = if aim.goals.len() == 1 { "" } else { "s" },
            goals = aim.describe(graph)?,
            judged = judgement(graph, start)?,
        )
    };
    let schema = schema(!backward);
    let prompt = prompt(reaction, &task, &known, &schema);
    let scratch = reaction.context().workspaces().scratch(ground)?;
    let response: Response =
        reaction.invoke(graph, Some(start.id), scratch.path(), &prompt, &schema)?;
    drop(scratch);

    let mut report = ExplorationReport {
        direction,
        from: start.id,
        proposed: Vec::new(),
        rejected: Vec::new(),
    };
    for proposal in response.nodes.into_iter().take(breadth) {
        let before = if backward {
            vec![start.id]
        } else {
            aim.resolve(&proposal.before)
        };
        record(
            graph,
            &mut report,
            &proposal.title,
            &proposal.objective,
            &after,
            &before,
        );
    }
    Ok(report)
}

/// What the step an exploration starts from did and how it was judged
/// (its latest verdict per goal), so the next steps can aim at where it
/// fell short; empty for the origin or a step never judged.
fn judgement(graph: &Graph, start: &Node) -> Result<String, RoleError> {
    if start.kind != NodeKind::Step {
        return Ok(String::new());
    }
    let mut latest: BTreeMap<NodeId, Evaluation> = BTreeMap::new();
    for evaluation in graph.evaluations_of(start.id)? {
        latest.insert(evaluation.goal, evaluation);
    }
    if latest.is_empty() {
        return Ok(String::new());
    }
    let mut out = format!(
        "\n## Where the work stands\n\n\
         The working directory already holds this step, carried out and judged:\n\n- {}\n",
        start.title
    );
    if let Some(outcome) = &start.outcome {
        let _ = writeln!(out, "  Outcome: {}", digest::excerpt(outcome));
    }
    for evaluation in latest.values() {
        let _ = writeln!(
            out,
            "  Judged {}/100 ({}): {}",
            evaluation.score,
            if evaluation.satisfied {
                "satisfied"
            } else {
                "not satisfied"
            },
            digest::excerpt(&evaluation.reason)
        );
    }
    out.push_str("\nAim the next steps at what it still falls short of.\n");
    Ok(out)
}

/// Explore past `failed`: new hypotheses in its place, building on what it
/// built on and heading for its goals, informed by what went wrong.
fn explore_failure(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    failed: &Node,
) -> Result<ExplorationReport, RoleError> {
    let origin = graph.origin()?;
    let lineage = graph.lineage()?;
    let predecessors = graph.predecessors(failed.id)?;
    let base = realised_base(&lineage, &origin, &predecessors.iter().collect::<Vec<_>>());
    let aim = Aim::from(graph, failed)?;
    let ground = match base.as_slice() {
        [only] if *only != origin.id => graph.node(*only)?,
        _ => origin.clone(),
    };
    let mut around = Vec::new();
    for id in &base {
        around.extend(graph.successors(*id)?);
    }
    let known = known_steps(graph, around)?;
    let breadth = breadth(reaction);

    let mut attempt = format!("- {}\n", failed.title);
    if !failed.objective.trim().is_empty() {
        let _ = writeln!(
            attempt,
            "  Objective: {}",
            digest::excerpt(&failed.objective)
        );
    }
    let _ = writeln!(
        attempt,
        "  What went wrong: {}",
        failed.outcome.as_deref().map_or_else(
            || "(not recorded)".to_owned(),
            |o| digest::excerpt_to(o, FAILURE_EXCERPT)
        )
    );
    if let Some(branch) = &failed.branch {
        let _ = writeln!(
            attempt,
            "  Whatever the attempt left is on branch `{branch}` (inspect it read-only, e.g. \
             `git log` / `git diff` against it)."
        );
    }
    let task = format!(
        "## Task\n\n\
         A step toward the goal{s} below was attempted and could not be carried out. A failure \
         is information: work out what it reveals — a wrong assumption, a step too large, a \
         missing prerequisite, an approach that cannot work here — and propose up to {breadth} \
         new hypotheses it suggests: alternatives that avoid the cause, smaller or preparatory \
         steps, investigations into why it failed, or a different way to reach the same aim. \
         Each will be attempted in place of the failed step, from the same starting point. The \
         working directory holds that starting point; read it to ground your proposals. Do not \
         modify files.\n\n\
         ## Failed step\n\n{attempt}\n\
         ## Goal{s}\n\n{goals}",
        s = if aim.goals.len() == 1 { "" } else { "s" },
        goals = aim.describe(graph)?,
    );
    let schema = schema(true);
    let prompt = prompt(reaction, &task, &known, &schema);
    let scratch = reaction.context().workspaces().scratch(&ground)?;
    let response: Response =
        reaction.invoke(graph, Some(failed.id), scratch.path(), &prompt, &schema)?;
    drop(scratch);

    let mut report = ExplorationReport {
        direction: Direction::Failure,
        from: failed.id,
        proposed: Vec::new(),
        rejected: Vec::new(),
    };
    for proposal in response.nodes.into_iter().take(breadth) {
        record(
            graph,
            &mut report,
            &proposal.title,
            &proposal.objective,
            &base,
            &aim.resolve(&proposal.before),
        );
    }
    Ok(report)
}

/// Explore ahead of `start`, a step not carried out yet: assume it and the
/// open steps it builds on succeed, and propose what could come after it.
/// `None` when it builds on a failed or pruned step, so it can never run.
fn explore_ahead(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    start: &Node,
) -> Result<Option<ExplorationReport>, RoleError> {
    // The open steps it rests on, and the realised nodes they start from.
    let mut assumed: Vec<Node> = vec![start.clone()];
    let mut frontier: Vec<Node> = Vec::new();
    let mut stack = vec![start.id];
    while let Some(id) = stack.pop() {
        for predecessor in graph.predecessors(id)? {
            match (predecessor.kind, predecessor.status) {
                (NodeKind::Origin, _) | (NodeKind::Step, NodeStatus::Completed) => {
                    if !frontier.iter().any(|n| n.id == predecessor.id) {
                        frontier.push(predecessor);
                    }
                }
                (NodeKind::Step, NodeStatus::Failed | NodeStatus::Pruned) => return Ok(None),
                (NodeKind::Step, _) => {
                    if !assumed.iter().any(|n| n.id == predecessor.id) {
                        stack.push(predecessor.id);
                        assumed.push(predecessor);
                    }
                }
                (NodeKind::Goal, _) => {}
            }
        }
    }
    assumed.sort_by_key(|n| n.id);
    let origin = graph.origin()?;
    let lineage = graph.lineage()?;
    let base = realised_base(&lineage, &origin, &frontier.iter().collect::<Vec<_>>());
    let ground = match base.as_slice() {
        [only] if *only != origin.id => graph.node(*only)?,
        _ => origin.clone(),
    };
    let aim = Aim::from(graph, start)?;
    let known = known_steps(graph, graph.successors(start.id)?)?;
    let breadth = breadth(reaction);

    let mut steps = String::new();
    for step in &assumed {
        let _ = writeln!(steps, "- {}", step.title);
        if !step.objective.trim().is_empty() {
            let _ = writeln!(steps, "  {}", digest::excerpt(&step.objective));
        }
    }
    let task = format!(
        "## Task\n\n\
         The steps below have not been carried out yet. Assume each of them succeeds as \
         described — the working directory holds the code they start from — and propose up \
         to {breadth} possible steps that could come right after the last of them, \
         \"{title}\", toward the goal{s} below. Read the code to ground your proposals in \
         what the assumed steps would change. Do not modify files.\n\n\
         ## Assumed done\n\n{steps}\n\
         ## Goal{s}\n\n{goals}",
        title = start.title,
        s = if aim.goals.len() == 1 { "" } else { "s" },
        goals = aim.describe(graph)?,
    );
    let schema = schema(true);
    let prompt = prompt(reaction, &task, &known, &schema);
    let scratch = reaction.context().workspaces().scratch(&ground)?;
    let response: Response =
        reaction.invoke(graph, Some(start.id), scratch.path(), &prompt, &schema)?;
    drop(scratch);

    let mut report = ExplorationReport {
        direction: Direction::Ahead,
        from: start.id,
        proposed: Vec::new(),
        rejected: Vec::new(),
    };
    for proposal in response.nodes.into_iter().take(breadth) {
        record(
            graph,
            &mut report,
            &proposal.title,
            &proposal.objective,
            &[start.id],
            &aim.resolve(&proposal.before),
        );
    }
    Ok(Some(report))
}

/// A graph-wide exploration's reaction to any event: survey toward the
/// goal just added, or toward every active goal — unless a survey started
/// after the event was recorded, which already saw what it brought.
fn survey_on(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    subject: &Node,
) -> Result<Report, RoleError> {
    let event = reaction.event();
    let aim = if event.kind == EventKind::GoalAdded {
        if subject.status != NodeStatus::Active {
            return Ok(Reaction::ignore(format!(
                "goal {} is {}",
                subject.id, subject.status
            )));
        }
        Aim::at(graph, subject)?
    } else {
        Aim::everywhere(graph)?
    };
    if aim.is_empty() {
        return Ok(Reaction::ignore("no active goals"));
    }
    if let Some(last) = graph.last_success(reaction.agent().name())?
        && last > event.created_at
    {
        return Ok(Reaction::ignore(format!(
            "the whole graph was surveyed at {last}, after this event"
        )));
    }
    Ok(Report::Exploration {
        explorations: vec![survey(reaction, graph, &aim)?],
    })
}

/// Survey the whole graph toward `goals`; the agent chooses where each
/// proposal attaches, within what can be built on.
fn survey(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    aim: &Aim,
) -> Result<ExplorationReport, RoleError> {
    let origin = graph.origin()?;
    let lineage = graph.lineage()?;
    let snapshot = graph.snapshot()?;
    let breadth = breadth(reaction);
    let task = format!(
        "## Task\n\n\
         Below is the whole map of the work toward the goals: every step proposed so far, what \
         it builds on (`after`) and heads for (`before`), what became of it, and how it was \
         judged. Look across the whole map rather than out from any one node, for what no single \
         node would suggest:\n\
         - gaps: parts of a goal no path addresses;\n\
         - combinations: separate paths whose results could be joined or built on together;\n\
         - patterns: causes shared by several failures or weak evaluations, and what would \
           remove them;\n\
         - assumptions every path shares that might be wrong;\n\
         - places nobody has explored from.\n\n\
         Propose up to {breadth} new steps. For each, give the ids of the nodes it builds on \
         (`after`: the origin {origin} or completed steps — several ids join their work) and of \
         the goals it heads toward (`before`). The working directory holds the repository at \
         the origin; read it to ground your proposals. Do not modify files.\n\n\
         ## Goals\n\n{goals}\n\
         ## Map\n\n{map}",
        origin = origin.id.0,
        goals = aim.describe(graph)?,
        map = digest::graph(&snapshot),
    );
    let schema = survey_schema();
    let prompt = prompt(reaction, &task, &[], &schema);
    let scratch = reaction.context().workspaces().scratch(&origin)?;
    let response: SurveyResponse =
        reaction.invoke(graph, None, scratch.path(), &prompt, &schema)?;
    drop(scratch);

    let mut report = ExplorationReport {
        direction: Direction::Survey,
        from: origin.id,
        proposed: Vec::new(),
        rejected: Vec::new(),
    };
    for proposal in response.nodes.into_iter().take(breadth) {
        let named: Vec<&Node> = proposal
            .after
            .iter()
            .filter_map(|id| snapshot.node(NodeId(*id)))
            .collect();
        let after = realised_base(&lineage, &origin, &named);
        let before = aim.resolve(&proposal.before);
        record(
            graph,
            &mut report,
            &proposal.title,
            &proposal.objective,
            &after,
            &before,
        );
    }
    Ok(report)
}

/// The prompt: the task (with what it needs to know), the steps that
/// already exist, the guidelines, and the response format.
fn prompt(
    reaction: &Reaction<'_>,
    task: &str,
    known: &[Node],
    schema: &serde_json::Value,
) -> Prompt {
    let known = if known.is_empty() {
        String::new()
    } else {
        let mut list = String::from(
            "\n## Already proposed\n\nThese steps already exist. Do not propose them again; \
             propose genuinely different alternatives, or what could come after them:\n\n",
        );
        for step in known {
            let _ = writeln!(list, "- {}", step.title);
        }
        list
    };
    Prompt::from(format!(
        "{task}{known}\n\
         Guidelines:\n\
         - Each step must be a concrete, verifiable unit of work that can be completed in one \
           session.\n\
         - Enumerate broadly: list every genuinely different possibility you can ground in \
           the code, including uncertain, unconventional, or risky ones. Do not filter by \
           likelihood; the steps are ranked and weeded out afterwards.\n\
         - Prefer genuinely different alternatives over one linear plan, and never repeat a \
           step that already exists.\n\
         {instructions}{response}",
        instructions = super::instructions_section(reaction.agent()),
        response = super::response_section(schema),
    ))
}
