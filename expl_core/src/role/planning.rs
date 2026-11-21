//! Planning — define paths of node combinations and set node priorities.
//!
//! Reacts to `node.proposed`, `node.evaluated`, `node.failed`, and
//! `goal.retired`, but always plans the whole graph: one reaction covers
//! every such event recorded before the [`Snapshot`] its agent is shown
//! (see [`Role::coalesces`](crate::role::Role::coalesces)). Steps proposed
//! while the agent works are neither planned nor covered; they wait, with
//! their events, for the next planning.
//!
//! Planning combines paths: a `join` makes an open step also build on other
//! steps (`A → C`, `B → C`), so its branch starts from their merge.
//! Exploration only ever proposes steps from one node; combinations are
//! planning's.
//!
//! Every proposed step the agent was shown leaves planning either planned
//! (with a priority) or pruned; shown steps the agent does not mention are
//! planned at their current priority, so a plan always settles the
//! proposals it saw — and only those. Planning a step
//! whose predecessors are completed records `node.ready`, which is what
//! sets execution in motion.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;

use crate::event::EventId;
use crate::graph::{
    GoalTree, Graph, GraphError, Node, NodeFilter, NodeId, NodeKind, NodeStatus, PlanId,
    PlannedPath, Snapshot,
};
use crate::prompt::Prompt;
use crate::role::{Reaction, Report, RoleError, digest};

/// What one planning did.
#[derive(Debug, Clone, Serialize)]
pub struct PlanningReport {
    /// The recorded plan.
    pub plan: PlanId,
    /// The agent's summary.
    pub summary: String,
    /// Number of paths recorded.
    pub paths: usize,
    /// Steps planned or re-prioritised.
    pub planned: Vec<NodeId>,
    /// Steps given further predecessors.
    pub joined: Vec<NodeId>,
    /// Steps moved off superseded origins onto the active one.
    pub rebased: Vec<NodeId>,
    /// Steps pruned.
    pub pruned: Vec<NodeId>,
    /// The newest event when the agent was shown the graph: the events
    /// this plan covers. Later ones are left for the next planning.
    pub covered: Option<EventId>,
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
    #[serde(default)]
    joins: Vec<JoinResponse>,
    #[serde(default)]
    rebases: Vec<i64>,
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
struct JoinResponse {
    node: i64,
    after: Vec<i64>,
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
        "required": ["summary", "paths", "priorities", "prunes", "joins", "rebases"],
        "properties": {
            "summary": {"type": "string", "description": "Your reading of the steps and the strategy behind this plan."},
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
            "joins": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["node", "after"],
                    "properties": {
                        "node": {"type": "integer", "description": "A proposed or planned step."},
                        "after": {"type": "array", "items": {"type": "integer"}, "description": "Further steps it builds on; its work starts from all of them combined."}
                    }
                }
            },
            "rebases": {
                "type": "array",
                "items": {"type": "integer"},
                "description": "Proposed/planned steps that build on a superseded origin and should start from the active origin instead."
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

pub(crate) fn react(reaction: &Reaction<'_>, graph: &mut Graph) -> Result<Report, RoleError> {
    let origin = graph.origin()?;
    let scratch = reaction.context().workspaces().scratch(&origin)?;
    let schema = schema();
    // What the agent is shown is all it plans: steps proposed while it
    // works are left, with their events, to the next planning.
    let shown = graph.snapshot()?;
    let prompt = prompt(reaction, &shown, &schema);
    let response: Response = reaction.invoke(graph, None, scratch.path(), &prompt, &schema)?;
    drop(scratch);

    let now = graph.nodes(&NodeFilter::default())?;
    let kind_of = |id: NodeId| shown.node(id).map(|n| n.kind);
    let is_open = |node: Option<&Node>| {
        node.is_some_and(|n| {
            n.kind == NodeKind::Step
                && matches!(n.status, NodeStatus::Proposed | NodeStatus::Planned)
        })
    };
    let open = |id: NodeId| is_open(shown.node(id)) && is_open(now.iter().find(|n| n.id == id));

    let paths: Vec<PlannedPath> = response
        .paths
        .into_iter()
        .filter(|p| kind_of(NodeId(p.goal)) == Some(NodeKind::Goal))
        .map(|p| PlannedPath {
            goal: NodeId(p.goal),
            nodes: p
                .nodes
                .into_iter()
                .map(NodeId)
                .filter(|id| kind_of(*id) == Some(NodeKind::Step))
                .collect(),
            rationale: p.rationale,
        })
        .collect();
    let plan = graph.record_plan(&response.summary, &paths)?;

    let mut pruned = Vec::new();
    for prune in response.prunes {
        let id = NodeId(prune.node);
        if open(id) {
            graph.prune(id, &format!("pruned by planning: {}", prune.reason))?;
            pruned.push(id);
        }
    }
    // Rebases before joins and priorities: where a step starts decides
    // what it can join and when it becomes ready.
    let mut rebased = Vec::new();
    for id in response.rebases.into_iter().map(NodeId) {
        if !open(id) || pruned.contains(&id) {
            continue;
        }
        match graph.rebase(id) {
            Ok(true) => rebased.push(id),
            // Nothing to move, or adopted work builds on it: the agent's
            // mistake, not a failed reaction.
            Ok(false) | Err(GraphError::NotInStatus { .. } | GraphError::Cycle { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    // Joins before priorities, so a step planned now becomes ready only
    // when its new predecessors are completed too.
    let mut joined = Vec::new();
    for join in response.joins {
        let id = NodeId(join.node);
        if !open(id) || pruned.contains(&id) {
            continue;
        }
        let after: Vec<NodeId> = join
            .after
            .into_iter()
            .map(NodeId)
            .filter(|source| kind_of(*source).is_some())
            .collect();
        if after.is_empty() {
            continue;
        }
        match graph.join(id, &after) {
            Ok(()) => joined.push(id),
            // A join the graph cannot hold is the agent's mistake, not a
            // failed reaction: the rest of the plan stands.
            Err(
                GraphError::NodeNotFound(_)
                | GraphError::NotInStatus { .. }
                | GraphError::InvalidEndpoint { .. }
                | GraphError::Cycle { .. },
            ) => {}
            Err(error) => return Err(error.into()),
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
    // Settle every proposal the agent saw but did not mention.
    for node in &shown.nodes {
        if node.kind == NodeKind::Step
            && node.status == NodeStatus::Proposed
            && open(node.id)
            && !pruned.contains(&node.id)
            && !planned.contains(&node.id)
        {
            graph.plan_step(node.id, node.priority)?;
            planned.insert(node.id);
        }
    }
    Ok(Report::Planning(PlanningReport {
        plan,
        summary: response.summary,
        paths: paths.len(),
        planned: planned.into_iter().collect(),
        joined,
        rebased,
        pruned,
        covered: shown.through,
    }))
}

/// The prompt: the goals and the steps toward them in `shown` — what
/// planning works on. Its previous plan is left out, so each plan starts
/// from the steps as they stand.
fn prompt(reaction: &Reaction<'_>, shown: &Snapshot, schema: &serde_json::Value) -> Prompt {
    Prompt::from(format!(
        "## Task\n\n\
         Below are goals and the steps that could lead to them: each step's state, what it \
         builds on (`after`) and leads to (`before`), and how completed steps were judged \
         against the goals. Decide where effort goes next.\n\n\
         - `paths`: for each goal, the most promising combinations of steps that could reach \
           it, in order. Several paths per goal are welcome; paths may share and merge steps.\n\
         - `priorities`: an integer for every `proposed` step and for any `planned` step whose \
           priority should change. Higher goes first. A planned step starts as soon as the \
           steps it builds on are completed.\n\
         - `joins`: combinations of paths. A `proposed`/`planned` step that should also build \
           on other steps lists them in `after`; its work then starts from all of them \
           combined. Join steps whose results complement each other.\n\
         - `prunes`: `proposed`/`planned` steps that are redundant, already covered, or no \
           longer promising given the judgements.\n\
         - `rebases`: `proposed`/`planned` steps that build on a superseded origin and should \
           start from the active origin instead. The trunk moving does not move them: move a \
           step when its idea still holds on the newer code; keep it where it is when it \
           depends on the older code (or is worth trying there as an alternative); prune it \
           when the newer code already covers it.\n\n\
         Origins are states the code has been in: the `active` origin is the code as it \
         stands now (the working directory), `superseded` ones are earlier states. Each origin \
         builds on the one before it and on the steps adopted into it, so their work is already \
         in. Steps that build on a superseded origin start from older code.\n\n\
         Steps are enumerated broadly, so many are weak, niche, or may even work against a \
         goal. Give those a low priority rather than pruning them, so the alternatives stay \
         on the map; prune only steps that duplicate another or are already covered. Balance \
         steps on paths whose judgements are improving with untried alternatives. Subgoals \
         are hypotheses about routes toward the goals they serve: favour paths through the \
         most promising subgoals, keep alternatives to them alive, and remember that a goal \
         can be reached without any of its subgoals. Do not modify files in the working \
         directory.\n\
         \n## Goals\n\n{goals}\n## Steps\n\n{graph}{instructions}{response}",
        goals = digest::numbered_goals(&shown.goals(), &GoalTree::of(&shown.nodes, &shown.edges)),
        graph = digest::graph(shown),
        instructions = super::instructions_section(reaction.agent()),
        response = super::response_section(schema),
    ))
}
