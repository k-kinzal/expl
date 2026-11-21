//! [`Planner`] — draws paths toward goals and sets node priorities.
//!
//! Every proposed step leaves planning either planned (with a priority) or
//! pruned; steps the planner does not mention are planned at priority 0, so
//! a plan always settles the proposals it saw. Priorities may also be
//! revised for steps that are already planned.

use std::collections::BTreeSet;
use std::fmt::Write;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::graph::{Graph, NodeFilter, NodeId, NodeKind, NodeStatus, PlanId, PlannedPath};
use crate::prompt::Prompt;
use crate::role::{Context, Role, RoleError, digest};

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct Planner<'a> {
    context: &'a Context,
}

/// What one planning did.
#[derive(Debug, Clone, Serialize)]
pub struct PlanningReport {
    /// The recorded plan.
    pub plan: PlanId,
    /// The planner's summary.
    pub summary: String,
    /// Number of paths recorded.
    pub paths: usize,
    /// Steps now planned (newly or re-prioritised).
    pub planned: Vec<NodeId>,
    /// Steps pruned.
    pub pruned: Vec<NodeId>,
}

#[derive(Debug, Deserialize)]
struct Response {
    summary: String,
    #[serde(default)]
    paths: Vec<PathResponse>,
    #[serde(default)]
    priorities: Vec<PriorityResponse>,
    #[serde(default)]
    prunes: Vec<PruneResponse>,
}

#[derive(Debug, Deserialize)]
struct PathResponse {
    goal: i64,
    nodes: Vec<i64>,
    rationale: String,
}

#[derive(Debug, Deserialize)]
struct PriorityResponse {
    node: i64,
    priority: i64,
}

#[derive(Debug, Deserialize)]
struct PruneResponse {
    node: i64,
    reason: String,
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "paths", "priorities", "prunes"],
        "properties": {
            "summary": {"type": "string", "description": "Your reading of the graph and the strategy behind this plan."},
            "paths": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["goal", "nodes", "rationale"],
                    "properties": {
                        "goal": {"type": "integer", "description": "Goal id."},
                        "nodes": {"type": "array", "items": {"type": "integer"}, "description": "Step ids in execution order (omit origin and goal)."},
                        "rationale": {"type": "string"}
                    }
                }
            },
            "priorities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["node", "priority"],
                    "properties": {
                        "node": {"type": "integer"},
                        "priority": {"type": "integer", "description": "Higher executes first."}
                    }
                }
            },
            "prunes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["node", "reason"],
                    "properties": {
                        "node": {"type": "integer"},
                        "reason": {"type": "string"}
                    }
                }
            }
        }
    })
}

impl<'a> Planner<'a> {
    /// A planner for `context`.
    #[must_use]
    pub fn new(context: &'a Context) -> Self {
        Self { context }
    }

    /// Plan once.
    ///
    /// # Errors
    ///
    /// Agent, graph, or workspace failures.
    pub fn plan(&self, graph: &mut Graph) -> Result<PlanningReport, RoleError> {
        let origin = graph.origin()?;
        let workspace = self.context.workspaces().open(&origin)?;
        let schema = schema();
        let prompt = self.prompt(graph, &schema)?;
        let result: Result<Response, RoleError> = self.context.invoke(
            graph,
            Role::Planner,
            None,
            workspace.path(),
            &prompt,
            &schema,
        );
        if let Some(commit) = &origin.head_commit {
            workspace.restore(commit)?;
        }
        let response = result?;

        let nodes = graph.nodes(&NodeFilter::default())?;
        let kind_of = |id: NodeId| {
            nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| (n.kind, n.status))
        };
        let open = |id: NodeId| {
            matches!(
                kind_of(id),
                Some((NodeKind::Step, NodeStatus::Proposed | NodeStatus::Planned))
            )
        };

        let paths: Vec<PlannedPath> = response
            .paths
            .into_iter()
            .filter(|p| matches!(kind_of(NodeId(p.goal)), Some((NodeKind::Goal, _))))
            .map(|p| PlannedPath {
                goal: NodeId(p.goal),
                nodes: p
                    .nodes
                    .into_iter()
                    .map(NodeId)
                    .filter(|id| matches!(kind_of(*id), Some((NodeKind::Step, _))))
                    .collect(),
                rationale: p.rationale,
            })
            .collect();
        let plan = graph.record_plan(&response.summary, &paths)?;

        let mut pruned = Vec::new();
        for prune in response.prunes {
            let id = NodeId(prune.node);
            if open(id) {
                graph.conclude(id, NodeStatus::Pruned, &format!("pruned: {}", prune.reason))?;
                pruned.push(id);
            }
        }
        let mut planned = BTreeSet::new();
        for priority in response.priorities {
            let id = NodeId(priority.node);
            if open(id) && !pruned.contains(&id) {
                graph.plan_step(id, priority.priority)?;
                planned.insert(id);
            }
        }
        // Settle every proposal the planner saw but did not mention.
        for node in &nodes {
            if node.kind == NodeKind::Step
                && node.status == NodeStatus::Proposed
                && !pruned.contains(&node.id)
                && !planned.contains(&node.id)
            {
                graph.plan_step(node.id, node.priority)?;
                planned.insert(node.id);
            }
        }
        Ok(PlanningReport {
            plan,
            summary: response.summary,
            paths: paths.len(),
            planned: planned.into_iter().collect(),
            pruned,
        })
    }

    fn prompt(&self, graph: &Graph, schema: &serde_json::Value) -> Result<Prompt, RoleError> {
        let current = match graph.latest_plan()? {
            Some(plan) => {
                let mut text = format!("Summary: {}\n", digest::excerpt(&plan.summary));
                for path in plan.paths {
                    let nodes: Vec<String> = path.nodes.iter().map(ToString::to_string).collect();
                    let _ = writeln!(
                        text,
                        "- toward {}: {} — {}",
                        path.goal,
                        nodes.join(" -> "),
                        digest::excerpt(&path.rationale)
                    );
                }
                text
            }
            None => "(no plan yet)\n".to_owned(),
        };
        let text = format!(
            "{preamble}\n## Task: plan\n\n\
             Read the graph and decide where execution effort goes next.\n\n\
             - `paths`: for each goal, the most promising combinations of steps that could reach \
               it, in execution order. Several paths per goal are welcome; paths may share and \
               merge steps.\n\
             - `priorities`: an integer for every `proposed` step and for any `planned` step \
               whose priority should change. Higher executes first. Steps whose predecessors are \
               not completed wait regardless of priority.\n\
             - `prunes`: `proposed`/`planned` steps that are redundant, already covered, or no \
               longer promising given the evaluations.\n\n\
             Balance exploitation (steps on paths whose evaluations are improving) with \
             exploration (untried alternatives). Your working directory is the origin's worktree; \
             do not modify files.\n\
             \n## Goals\n\n{goals}\n## Current plan\n\n{current}\n## Graph\n\n{graph}\
             {instructions}{response}",
            preamble = super::preamble(
                Role::Planner,
                "Your responsibility is planning: define paths of node combinations found by \
                 exploration, and set node priorities."
            ),
            goals = digest::goals(&graph.goals()?),
            graph = digest::graph(graph)?,
            instructions = super::instructions_section(self.context, Role::Planner),
            response = super::response_section(schema),
        );
        Ok(Prompt::from(text))
    }
}
