//! Evaluation — judge a completed step and determine whether goals are
//! satisfied.
//!
//! Reacts to `node.completed`. The step is judged against the active goals
//! it leads to (or every active goal when it is not linked to any).
//!
//! Reacts to `origin.advanced` too: the new origin — the trunk as the
//! owner left it — is judged against every active goal, so the lineage
//! shows how each adoption moved each goal. An origin's verdicts never make
//! it a candidate; candidates are steps. The
//! agent works in a scratch worktree at the step's commit, so it can build
//! and test the result without disturbing anyone; its prompt holds only the
//! goals. Recording the verdicts
//! records `node.evaluated`, which sets exploration (what next from here?)
//! and planning (re-prioritise) in motion.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::event::EventKind;
use crate::graph::{GoalTree, Graph, NewEvaluation, Node, NodeId, NodeKind, NodeStatus};
use crate::prompt::Prompt;
use crate::role::{Reaction, Report, RoleError, digest};

/// What one evaluation did.
#[derive(Debug, Clone, Serialize)]
pub struct EvaluationReport {
    /// The evaluated step.
    pub node: NodeId,
    /// Verdicts recorded, one per goal.
    pub verdicts: Vec<Verdict>,
}

/// One recorded verdict.
#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    /// The goal.
    pub goal: NodeId,
    /// Whether it is satisfied.
    pub satisfied: bool,
    /// Progress, 0–100.
    pub score: i64,
}

#[derive(Debug, Deserialize)]
struct Response {
    evaluations: Vec<EvaluationResponse>,
}

#[derive(Debug, Deserialize)]
struct EvaluationResponse {
    goal: i64,
    satisfied: bool,
    score: i64,
    reason: String,
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["evaluations"],
        "properties": {
            "evaluations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["goal", "satisfied", "score", "reason"],
                    "properties": {
                        "goal": {"type": "integer", "description": "The goal's number in brackets."},
                        "satisfied": {"type": "boolean", "description": "Whether the repository, as it is, satisfies the goal."},
                        "score": {"type": "integer", "minimum": 0, "maximum": 100, "description": "Progress toward the goal."},
                        "reason": {"type": "string", "description": "Evidence for the verdict, and what is still missing."}
                    }
                }
            }
        }
    })
}

pub(crate) fn react(reaction: &Reaction<'_>, graph: &mut Graph) -> Result<Report, RoleError> {
    let id = reaction.subject()?;
    let node = graph.node(id)?;
    let judged = if reaction.event().kind == EventKind::OriginAdvanced {
        node.kind == NodeKind::Origin
    } else {
        node.kind == NodeKind::Step && node.status == NodeStatus::Completed
    };
    if !judged {
        return Ok(Reaction::ignore(format!(
            "{id} is a {} {}",
            node.status, node.kind
        )));
    }
    let mut goals = if node.kind == NodeKind::Origin {
        Vec::new()
    } else {
        graph.goals_reachable_from(id)?
    };
    if goals.is_empty() {
        goals = graph.goals()?;
    }

    let scratch = reaction.context().workspaces().scratch(&node)?;
    let evaluations: Vec<NewEvaluation> = if goals.is_empty() {
        Vec::new()
    } else {
        let schema = schema();
        let tree = graph.goal_tree()?;
        let prompt = prompt(reaction, &goals, &tree, &schema);
        let response: Response =
            reaction.invoke(graph, Some(id), scratch.path(), &prompt, &schema)?;
        response
            .evaluations
            .into_iter()
            .filter(|e| goals.iter().any(|g| g.id.0 == e.goal))
            .map(|e| NewEvaluation {
                goal: NodeId(e.goal),
                satisfied: e.satisfied,
                score: e.score.clamp(0, 100),
                reason: e.reason,
            })
            .collect()
    };
    drop(scratch);
    graph.record_evaluations(id, &evaluations)?;
    Ok(Report::Evaluation(EvaluationReport {
        node: id,
        verdicts: evaluations
            .iter()
            .map(|e| Verdict {
                goal: e.goal,
                satisfied: e.satisfied,
                score: e.score,
            })
            .collect(),
    }))
}

/// The prompt: the goals to judge against — not the step, its report, or
/// the path that led to it. The working directory is the evidence.
fn prompt(
    reaction: &Reaction<'_>,
    goals: &[Node],
    tree: &GoalTree,
    schema: &serde_json::Value,
) -> Prompt {
    Prompt::from(format!(
        "## Task\n\n\
         Judge the repository in the working directory, as it is, against each goal below. \
         Inspect the code and run builds and tests as needed. Do not modify files. Return one \
         evaluation per goal, referring to it by the number in brackets.\n\n\
         ## Goals\n\n{goals}{instructions}{response}",
        goals = digest::numbered_goals(goals, tree),
        instructions = super::instructions_section(reaction.agent()),
        response = super::response_section(schema),
    ))
}
