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
//! Every exploration is told which steps already exist around it (the
//! ones built on its starting point, and all open work), so a repeated
//! exploration proposes alternatives instead of the same steps again.
//!
//! The agent works read-only in a scratch worktree at the node it explores
//! from (the origin, for backward exploration), so its proposals are
//! grounded in the actual code. Its prompt holds only the goals it explores
//! toward; where proposals attach in the graph is decided here, not by the
//! agent.

use std::fmt::Write;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::event::EventKind;
use crate::graph::{Graph, Node, NodeFilter, NodeId, NodeKind, NodeStatus};
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
}

fn schema() -> serde_json::Value {
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
                    "required": ["title", "objective"],
                    "properties": {
                        "title": {"type": "string", "description": "Short imperative name of the step."},
                        "objective": {"type": "string", "description": "What the step must achieve, concretely enough to be done in one session and checked afterwards."}
                    }
                }
            }
        }
    })
}

pub(crate) fn react(reaction: &Reaction<'_>, graph: &mut Graph) -> Result<Report, RoleError> {
    let subject = graph.node(reaction.subject()?)?;
    let explorations = match reaction.event().kind {
        EventKind::GoalAdded => {
            if subject.status != NodeStatus::Active {
                return Ok(Reaction::ignore(format!(
                    "goal {} is {}",
                    subject.id, subject.status
                )));
            }
            let origin = graph.origin()?;
            let goals = [subject.clone()];
            let mut explorations = vec![explore(
                reaction,
                graph,
                Direction::Forward,
                &origin,
                &goals,
            )?];
            if subject.counter_exploration {
                explorations.push(explore(
                    reaction,
                    graph,
                    Direction::Backward,
                    &subject,
                    &goals,
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
            let goals = graph.goals()?;
            if goals.is_empty() {
                return Ok(Reaction::ignore("no active goals"));
            }
            let mut explorations = vec![explore(
                reaction,
                graph,
                Direction::Forward,
                &subject,
                &goals,
            )?];
            for goal in goals.iter().filter(|g| g.counter_exploration) {
                explorations.push(explore(
                    reaction,
                    graph,
                    Direction::Backward,
                    goal,
                    std::slice::from_ref(goal),
                )?);
            }
            explorations
        }
        EventKind::NodeRevisited => {
            if !graph.starting_points()?.iter().any(|n| n.id == subject.id) {
                return Ok(Reaction::ignore(format!(
                    "{} ({} {}) is no longer a starting point",
                    subject.id, subject.status, subject.kind
                )));
            }
            if subject.kind == NodeKind::Goal {
                let goals = [subject.clone()];
                vec![explore(
                    reaction,
                    graph,
                    Direction::Backward,
                    &subject,
                    &goals,
                )?]
            } else {
                let goals = goals_from(graph, &subject)?;
                vec![explore(
                    reaction,
                    graph,
                    Direction::Forward,
                    &subject,
                    &goals,
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
            let goals = goals_from(graph, &subject)?;
            if goals.is_empty() {
                return Ok(Reaction::ignore("no active goals"));
            }
            vec![explore(
                reaction,
                graph,
                Direction::Forward,
                &subject,
                &goals,
            )?]
        }
        other @ (EventKind::GoalRetired
        | EventKind::NodeProposed
        | EventKind::NodePlanned
        | EventKind::NodeReady
        | EventKind::NodeStarted
        | EventKind::NodeCompleted
        | EventKind::NodeFailed
        | EventKind::NodePruned
        | EventKind::PlanRecorded) => {
            return Ok(Reaction::ignore(format!(
                "exploration does not react to {other}"
            )));
        }
    };
    Ok(Report::Exploration { explorations })
}

/// The active goals a step leads to, or every active goal when it leads
/// to none (the origin, or a step whose goals were retired).
fn goals_from(graph: &Graph, start: &Node) -> Result<Vec<Node>, RoleError> {
    let goals: Vec<Node> = graph
        .goals_reachable_from(start.id)?
        .into_iter()
        .filter(|goal| goal.status == NodeStatus::Active)
        .collect();
    if goals.is_empty() {
        return Ok(graph.goals()?);
    }
    Ok(goals)
}

/// Most steps an exploration is told already exist.
const KNOWN_STEPS: usize = 40;

/// The steps that already exist around an exploration from `start`: those
/// built on it (forward) or leading to it (backward), whatever became of
/// them, and every step still open anywhere — newest first.
fn known_steps(graph: &Graph, direction: Direction, start: &Node) -> Result<Vec<Node>, RoleError> {
    let mut steps = match direction {
        Direction::Forward => graph.successors(start.id)?,
        Direction::Backward => graph.predecessors(start.id)?,
    };
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

/// Explore from `start` toward `goals`. Forward proposals build on `start`
/// and lead to `goals`; backward proposals (`start` is the goal) build on
/// the origin and lead to `start`.
fn explore(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    direction: Direction,
    start: &Node,
    goals: &[Node],
) -> Result<ExplorationReport, RoleError> {
    let origin = graph.origin()?;
    let (ground, after, before): (&Node, Vec<NodeId>, Vec<NodeId>) = match direction {
        Direction::Forward => (start, vec![start.id], goals.iter().map(|g| g.id).collect()),
        Direction::Backward => (&origin, vec![origin.id], vec![start.id]),
    };
    let scratch = reaction.context().workspaces().scratch(ground)?;
    let breadth = reaction.agent().declaration().breadth.unwrap_or(1);
    let known = known_steps(graph, direction, start)?;
    let schema = schema();
    let prompt = prompt(reaction, direction, start, goals, &known, breadth, &schema);
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
        match graph.propose_step(
            proposal.title.trim(),
            proposal.objective.trim(),
            &after,
            &before,
        ) {
            Ok(id) => report.proposed.push(id),
            Err(error) => report.rejected.push(Rejection {
                title: proposal.title,
                reason: error.to_string(),
            }),
        }
    }
    Ok(report)
}

/// The prompt: the goals, the task, and the steps that already exist —
/// nothing else about the graph.
fn prompt(
    reaction: &Reaction<'_>,
    direction: Direction,
    start: &Node,
    goals: &[Node],
    known: &[Node],
    breadth: usize,
    schema: &serde_json::Value,
) -> Prompt {
    let task = match direction {
        Direction::Forward => format!(
            "## Task\n\n\
             Propose up to {breadth} possible next steps that could move this repository toward \
             the goal{s} below. The working directory holds the repository as it is now; read it to ground \
             your proposals. Do not modify files.\n\n\
             ## Goal{s}\n\n{goals}",
            s = if goals.len() == 1 { "" } else { "s" },
            goals = digest::goals(goals),
        ),
        Direction::Backward => format!(
            "## Task\n\n\
             The target below is a state this repository should reach. Work backward from it: \
             propose up to {breadth} possible steps that could be realised immediately before \
             the target is reached. The working directory holds the repository as it is now; read it to \
             ground your proposals. Do not modify files.\n\n\
             ## Target\n\n{target}",
            target = digest::goals(std::slice::from_ref(start)),
        ),
    };
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
