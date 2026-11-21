//! [`Evaluator`] — judges a completed step against goals.
//!
//! The step is judged against the active goals it leads to (or every active
//! goal when it is not linked to any). The evaluator runs in the step's
//! worktree so it can inspect and test the result; the tree is restored to
//! the recorded tip afterwards.

use std::fmt::Write;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::graph::{Graph, NewEvaluation, Node, NodeId, NodeKind, NodeStatus};
use crate::prompt::Prompt;
use crate::role::{Context, Role, RoleError, digest};

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct Evaluator<'a> {
    context: &'a Context,
}

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
                        "goal": {"type": "integer", "description": "Goal id."},
                        "satisfied": {"type": "boolean", "description": "Whether this branch, as is, satisfies the goal."},
                        "score": {"type": "integer", "minimum": 0, "maximum": 100, "description": "Progress toward the goal."},
                        "reason": {"type": "string", "description": "Evidence for the verdict, and what is still missing."}
                    }
                }
            }
        }
    })
}

impl<'a> Evaluator<'a> {
    /// An evaluator for `context`.
    #[must_use]
    pub fn new(context: &'a Context) -> Self {
        Self { context }
    }

    /// Evaluate the completed step `id`.
    ///
    /// # Errors
    ///
    /// [`RoleError::NotApplicable`] when `id` is not a completed step;
    /// agent, graph, or workspace failures.
    pub fn evaluate(&self, graph: &mut Graph, id: NodeId) -> Result<EvaluationReport, RoleError> {
        let node = graph.node(id)?;
        let Some(commit) = node
            .head_commit
            .clone()
            .filter(|_| node.kind == NodeKind::Step && node.status == NodeStatus::Completed)
        else {
            return Err(RoleError::NotApplicable {
                node: id,
                role: "evaluator",
                reason: format!("it is a {} {} node", node.status, node.kind),
            });
        };
        let mut goals = graph.goals_reachable_from(id)?;
        if goals.is_empty() {
            goals = graph.goals()?;
        }
        if goals.is_empty() {
            return Ok(EvaluationReport {
                node: id,
                verdicts: Vec::new(),
            });
        }

        let workspace = self.context.workspaces().open(&node)?;
        workspace.restore(&commit)?;
        let origin = graph.origin()?;
        let diff = origin
            .head_commit
            .as_deref()
            .map(|base| workspace.diff_stat(base))
            .transpose()?
            .unwrap_or_default();
        let schema = schema();
        let prompt = self.prompt(graph, &node, &goals, &diff, &schema)?;
        let result: Result<Response, RoleError> = self.context.invoke(
            graph,
            Role::Evaluator,
            Some(id),
            workspace.path(),
            &prompt,
            &schema,
        );
        workspace.restore(&commit)?;
        let response = result?;

        let evaluations: Vec<NewEvaluation> = response
            .evaluations
            .into_iter()
            .filter(|e| goals.iter().any(|g| g.id.0 == e.goal))
            .map(|e| NewEvaluation {
                goal: NodeId(e.goal),
                satisfied: e.satisfied,
                score: e.score.clamp(0, 100),
                reason: e.reason,
            })
            .collect();
        graph.record_evaluations(id, &evaluations)?;
        Ok(EvaluationReport {
            node: id,
            verdicts: evaluations
                .iter()
                .map(|e| Verdict {
                    goal: e.goal,
                    satisfied: e.satisfied,
                    score: e.score,
                })
                .collect(),
        })
    }

    fn prompt(
        &self,
        graph: &Graph,
        node: &Node,
        goals: &[Node],
        diff: &str,
        schema: &serde_json::Value,
    ) -> Result<Prompt, RoleError> {
        let mut history = String::new();
        for ancestor in graph.ancestors(node.id)? {
            let _ = writeln!(history, "- {}", digest::headline(&ancestor));
        }
        let text = format!(
            "{preamble}\n## Task: evaluate {id}\n\n\
             Step: {title}\nObjective: {objective}\n\nExecutor's report:\n{outcome}\n\n\
             Path to this step:\n{history}\n\
             Changes since the origin (`git diff --stat`):\n```\n{diff}\n```\n\n\
             Your working directory is the step's worktree (branch `{branch}`). Inspect the code, \
             run builds and tests as needed, and judge the branch **as it is** against each goal \
             below — not the executor's claims. Do not modify files; the worktree is reset after \
             you finish. Return one evaluation per goal.\n\
             \n## Goals to judge against\n\n{goals}{instructions}{response}",
            preamble = super::preamble(
                Role::Evaluator,
                "Your responsibility is evaluation: judge completed nodes and determine whether \
                 a goal is satisfied."
            ),
            id = node.id,
            title = node.title,
            objective = node.objective,
            outcome = node.outcome.as_deref().unwrap_or("(none)"),
            history = if history.is_empty() {
                "(starts at the origin)\n".to_owned()
            } else {
                history
            },
            branch = node.branch.as_deref().unwrap_or("?"),
            goals = digest::goals(goals),
            instructions = super::instructions_section(self.context, Role::Evaluator),
            response = super::response_section(schema),
        );
        Ok(Prompt::from(text))
    }
}
