//! [`Explorer`] — proposes nodes that could lead toward the goals.
//!
//! Forward exploration starts at a realised node (the origin or a completed
//! step) and asks what could come next. Backward exploration —
//! counter-exploration — starts at a goal and asks what must hold just
//! before it. Either way the explorer runs read-only in a worktree so its
//! proposals are grounded in the actual code; whatever it changes there is
//! discarded.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::graph::{Graph, NodeFilter, NodeId, NodeKind, NodeStatus};
use crate::prompt::Prompt;
use crate::role::{Context, Direction, Role, RoleError, digest};

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct Explorer<'a> {
    context: &'a Context,
}

/// What one exploration did.
#[derive(Debug, Clone, Serialize)]
pub struct ExplorationReport {
    /// Search direction.
    pub direction: Direction,
    /// The node explored from, or `None` when there was nothing to explore.
    pub from: Option<NodeId>,
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
    #[serde(default)]
    after: Vec<i64>,
    #[serde(default)]
    before: Vec<i64>,
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
                    "required": ["title", "objective", "after", "before"],
                    "properties": {
                        "title": {"type": "string", "description": "Short imperative name of the step."},
                        "objective": {"type": "string", "description": "What the step must achieve, concretely enough for an executor to do it in one session and for an evaluator to check it."},
                        "after": {"type": "array", "items": {"type": "integer"}, "description": "Ids of existing origin/step nodes this step builds on."},
                        "before": {"type": "array", "items": {"type": "integer"}, "description": "Ids of existing step/goal nodes this step leads toward."}
                    }
                }
            }
        }
    })
}

impl<'a> Explorer<'a> {
    /// An explorer for `context`.
    #[must_use]
    pub fn new(context: &'a Context) -> Self {
        Self { context }
    }

    /// The node a forward exploration should start from when none is
    /// given: among realised nodes, the one with the fewest live successors
    /// (breadth first), then the best evaluated, then the newest.
    ///
    /// # Errors
    ///
    /// [`RoleError::Graph`] on graph failure.
    pub fn frontier(graph: &Graph) -> Result<Option<NodeId>, RoleError> {
        let realised = graph.nodes(&NodeFilter {
            kinds: vec![NodeKind::Origin, NodeKind::Step],
            statuses: vec![NodeStatus::Completed],
        })?;
        let evaluations = graph.evaluations()?;
        let mut best: Option<((usize, i64, i64), NodeId)> = None;
        for node in realised {
            let successors = graph
                .successors(node.id)?
                .into_iter()
                .filter(|s| s.kind == NodeKind::Step && s.status != NodeStatus::Pruned)
                .count();
            let score = evaluations
                .iter()
                .filter(|e| e.node == node.id)
                .map(|e| e.score)
                .max()
                .unwrap_or(0);
            // Smaller key wins: fewer successors, higher score, newer id.
            let key = (successors, -score, -node.id.0);
            if best.as_ref().is_none_or(|(k, _)| key < *k) {
                best = Some((key, node.id));
            }
        }
        Ok(best.map(|(_, id)| id))
    }

    /// The goal a backward exploration should start from when none is
    /// given: the counter-exploration goal with the fewest predecessors.
    ///
    /// # Errors
    ///
    /// [`RoleError::Graph`] on graph failure.
    pub fn counter_frontier(graph: &Graph) -> Result<Option<NodeId>, RoleError> {
        let mut best: Option<(usize, NodeId)> = None;
        for goal in graph.goals()?.into_iter().filter(|g| g.counter_exploration) {
            let count = graph.predecessors(goal.id)?.len();
            if best.as_ref().is_none_or(|(c, _)| count < *c) {
                best = Some((count, goal.id));
            }
        }
        Ok(best.map(|(_, id)| id))
    }

    /// Explore once in `direction`, from `from` or the frontier.
    ///
    /// # Errors
    ///
    /// [`RoleError::NotApplicable`] when `from` cannot start this
    /// direction; agent, graph, or workspace failures otherwise.
    pub fn explore(
        &self,
        graph: &mut Graph,
        direction: Direction,
        from: Option<NodeId>,
    ) -> Result<ExplorationReport, RoleError> {
        let from = match from {
            Some(id) => Some(id),
            None => match direction {
                Direction::Forward => Self::frontier(graph)?,
                Direction::Backward => Self::counter_frontier(graph)?,
            },
        };
        let Some(from) = from else {
            return Ok(ExplorationReport {
                direction,
                from: None,
                proposed: Vec::new(),
                rejected: Vec::new(),
            });
        };
        let start = graph.node(from)?;
        let origin = graph.origin()?;
        let not_applicable = |reason: &str| RoleError::NotApplicable {
            node: from,
            role: "explorer",
            reason: reason.to_owned(),
        };
        let ground = match direction {
            Direction::Forward => {
                if start.kind == NodeKind::Goal || start.status != NodeStatus::Completed {
                    return Err(not_applicable(
                        "forward exploration starts from the origin or a completed step",
                    ));
                }
                start.clone()
            }
            Direction::Backward => {
                if start.kind == NodeKind::Origin {
                    return Err(not_applicable(
                        "backward exploration starts from a goal or a step",
                    ));
                }
                origin.clone()
            }
        };

        let workspace = self.context.workspaces().open(&ground)?;
        let breadth = self.context.declaration().run.breadth;
        let schema = schema();
        let prompt = self.prompt(graph, direction, &start, origin.id, breadth, &schema)?;
        let result: Result<Response, RoleError> = self.context.invoke(
            graph,
            Role::Explorer,
            Some(from),
            workspace.path(),
            &prompt,
            &schema,
        );
        if let Some(commit) = &ground.head_commit {
            workspace.restore(commit)?;
        }
        let response = result?;

        let mut report = ExplorationReport {
            direction,
            from: Some(from),
            proposed: Vec::new(),
            rejected: Vec::new(),
        };
        for proposal in response.nodes.into_iter().take(breadth) {
            let mut after: Vec<NodeId> = proposal.after.into_iter().map(NodeId).collect();
            let mut before: Vec<NodeId> = proposal.before.into_iter().map(NodeId).collect();
            match direction {
                Direction::Forward if after.is_empty() => after.push(from),
                Direction::Backward if before.is_empty() => before.push(from),
                Direction::Forward | Direction::Backward => {}
            }
            if after.is_empty() {
                after.push(origin.id);
            }
            after.dedup();
            before.dedup();
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

    fn prompt(
        &self,
        graph: &Graph,
        direction: Direction,
        start: &crate::graph::Node,
        origin: NodeId,
        breadth: usize,
        schema: &serde_json::Value,
    ) -> Result<Prompt, RoleError> {
        let task = match direction {
            Direction::Forward => format!(
                "## Task: forward exploration from {id}\n\n\
                 Starting node: {headline}\n\n\
                 Propose up to {breadth} new steps that could follow {id} and move the repository \
                 toward the goals. Your working directory is a git worktree at the state of {id}; \
                 read the code to ground your proposals. Do not modify files — anything you \
                 change is discarded.\n\n\
                 - `after`: existing origin/step ids the step builds on. Empty means {id}. List \
                   several to merge their branches.\n\
                 - `before`: existing step/goal ids the step leads toward. Include the goal(s) it \
                   advances.",
                id = start.id,
                headline = digest::headline(start),
            ),
            Direction::Backward => format!(
                "## Task: counter-exploration from {id}\n\n\
                 Target node: {headline}\n    {objective}\n\n\
                 Work backward: propose up to {breadth} new steps that must be realised \
                 immediately before {id} can be reached. Your working directory is a git worktree \
                 at the origin's state; read the code to ground your proposals. Do not modify \
                 files — anything you change is discarded.\n\n\
                 - `before`: existing step/goal ids the step is a precondition of. Empty means \
                   {id}.\n\
                 - `after`: existing origin/step ids the step builds on. Empty means the origin \
                   ({origin}).",
                id = start.id,
                headline = digest::headline(start),
                objective = digest::excerpt(&start.objective),
            ),
        };
        let text = format!(
            "{preamble}\n{task}\n\n\
             Guidelines:\n\
             - Each step must be a concrete, verifiable unit of work that one agent can complete \
               in one session, ending in a commit.\n\
             - Prefer genuinely different alternatives over one linear plan: breadth matters, and \
               paths may later merge.\n\
             - Do not duplicate existing nodes; refine or branch from them instead.\n\
             - Propose fewer steps (even none) rather than weak ones.\n\
             \n## Goals\n\n{goals}\n## Graph\n\n{graph}{instructions}{response}",
            preamble = super::preamble(
                Role::Explorer,
                "Your responsibility is exploration: identify nodes that could lead from a point in \
                 the graph to a goal."
            ),
            goals = digest::goals(&graph.goals()?),
            graph = digest::graph(graph)?,
            instructions = super::instructions_section(self.context, Role::Explorer),
            response = super::response_section(schema),
        );
        Ok(Prompt::from(text))
    }
}
