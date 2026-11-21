//! [`Executor`] — realises one step in its own worktree.
//!
//! The step's branch starts from its predecessors (merged when paths join),
//! the agent works in the worktree, and expl commits whatever it left there
//! as the branch tip. A failed run still commits its work so the evaluator
//! and the human can inspect it; the step is marked failed.

use std::fmt::Write;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::graph::{Graph, Node, NodeId, NodeKind, NodeStatus};
use crate::prompt::Prompt;
use crate::role::{Context, Role, RoleError, digest};
use crate::workspace::{Workspace, Workspaces};

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct Executor<'a> {
    context: &'a Context,
}

/// What one execution did.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionReport {
    /// The step.
    pub node: NodeId,
    /// `completed` or `failed`.
    pub status: NodeStatus,
    /// Branch holding the work.
    pub branch: String,
    /// Tip commit, when the work could be committed.
    pub head_commit: Option<String>,
    /// The executor's summary or the failure.
    pub summary: String,
}

#[derive(Debug, Deserialize)]
struct Response {
    succeeded: bool,
    summary: String,
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["succeeded", "summary"],
        "properties": {
            "succeeded": {"type": "boolean", "description": "Whether the step's objective is achieved in the worktree."},
            "summary": {"type": "string", "description": "What you changed, how you verified it, and anything left undone."}
        }
    })
}

impl<'a> Executor<'a> {
    /// An executor for `context`.
    #[must_use]
    pub fn new(context: &'a Context) -> Self {
        Self { context }
    }

    /// Claim and execute the planned step `id`.
    ///
    /// Failures of the work itself (agent failure, unresolved conflicts) are
    /// recorded on the node and reported with status `failed`; only
    /// infrastructure failures are `Err`.
    ///
    /// # Errors
    ///
    /// [`RoleError::NotApplicable`] when `id` is not a ready planned step;
    /// graph failures.
    pub fn execute(&self, graph: &mut Graph, id: NodeId) -> Result<ExecutionReport, RoleError> {
        let node = graph.node(id)?;
        let not_applicable = |reason: String| RoleError::NotApplicable {
            node: id,
            role: "executor",
            reason,
        };
        if node.kind != NodeKind::Step {
            return Err(not_applicable(format!("it is a {} node", node.kind)));
        }
        let mut predecessors = graph.predecessors(id)?;
        if let Some(blocking) = predecessors
            .iter()
            .find(|p| p.status != NodeStatus::Completed)
        {
            return Err(not_applicable(format!(
                "predecessor {} is {}",
                blocking.id, blocking.status
            )));
        }
        if predecessors.is_empty() {
            predecessors.push(graph.origin()?);
        }
        if !graph.claim(id)? {
            return Err(not_applicable(format!(
                "it is {}, not planned",
                node.status
            )));
        }

        let branch = Workspaces::branch_name(id);
        let workspace = match self.context.workspaces().create(&node, &predecessors) {
            Ok(workspace) => workspace,
            Err(error) => return Self::fail(graph, &node, &branch, None, error.to_string()),
        };
        graph.set_branch(id, &branch, None)?;

        let schema = schema();
        let prompt = self.prompt(graph, &node, &predecessors, &workspace, &schema)?;
        let result: Result<Response, RoleError> = self.context.invoke(
            graph,
            Role::Executor,
            Some(id),
            workspace.path(),
            &prompt,
            &schema,
        );
        let (succeeded, summary) = match result {
            Ok(response) => (response.succeeded, response.summary),
            Err(error @ (RoleError::Agent(_) | RoleError::Response { .. })) => {
                (false, format!("executor agent failed: {error}"))
            }
            Err(error) => {
                graph.conclude(id, NodeStatus::Failed, &error.to_string())?;
                return Err(error);
            }
        };

        let message = format!("expl: {} {}\n\n{}", id, node.title, summary);
        let head = match workspace.commit(&message) {
            Ok(head) => head,
            Err(error) => {
                return Self::fail(graph, &node, &branch, None, format!("{summary}\n\n{error}"));
            }
        };
        graph.set_branch(id, &branch, Some(&head))?;
        if succeeded {
            graph.conclude(id, NodeStatus::Completed, &summary)?;
            Ok(ExecutionReport {
                node: id,
                status: NodeStatus::Completed,
                branch,
                head_commit: Some(head),
                summary,
            })
        } else {
            Self::fail(graph, &node, &branch, Some(head), summary)
        }
    }

    fn fail(
        graph: &Graph,
        node: &Node,
        branch: &str,
        head_commit: Option<String>,
        summary: String,
    ) -> Result<ExecutionReport, RoleError> {
        graph.conclude(node.id, NodeStatus::Failed, &summary)?;
        Ok(ExecutionReport {
            node: node.id,
            status: NodeStatus::Failed,
            branch: branch.to_owned(),
            head_commit,
            summary,
        })
    }

    fn prompt(
        &self,
        graph: &Graph,
        node: &Node,
        predecessors: &[Node],
        workspace: &Workspace,
        schema: &serde_json::Value,
    ) -> Result<Prompt, RoleError> {
        let mut history = String::new();
        for ancestor in graph.ancestors(node.id)? {
            let _ = writeln!(history, "- {}", digest::headline(&ancestor));
            if let Some(outcome) = &ancestor.outcome {
                let _ = writeln!(history, "  outcome: {}", digest::excerpt(outcome));
            }
        }
        let joined: Vec<String> = predecessors.iter().map(|p| p.id.to_string()).collect();
        let conflicts = if workspace.conflicts().is_empty() {
            String::new()
        } else {
            format!(
                "\n**Merge conflicts.** Joining the predecessor branches left conflicts in: {}. \
                 Resolve them first (remove all conflict markers); expl concludes the merge when \
                 it commits.\n",
                workspace.conflicts().join(", ")
            )
        };
        let mut targets = String::new();
        for goal in graph.goals_reachable_from(node.id)? {
            let _ = writeln!(
                targets,
                "- {} {}: {}",
                goal.id,
                goal.title,
                digest::excerpt(&goal.objective)
            );
        }
        if targets.is_empty() {
            targets = digest::goals(&graph.goals()?);
        }
        let text = format!(
            "{preamble}\n## Task: execute {id}\n\n\
             Title: {title}\n\nObjective:\n{objective}\n\n\
             Your working directory is a git worktree on branch `{branch}`, started from {joined}. \
             Do the work there: edit files, run builds and tests, and verify the objective. \
             Do not commit, switch branches, or touch other worktrees — expl commits everything \
             you leave in this worktree when you finish.\n{conflicts}\
             \n## Goals this step serves\n\n{targets}\n## Path so far\n\n{history}\
             {instructions}\n\nReport `succeeded: false` honestly if the objective is not \
             achieved.\n{response}",
            preamble = super::preamble(
                Role::Executor,
                "Your responsibility is execution: realise one node of the graph."
            ),
            id = node.id,
            title = node.title,
            objective = node.objective,
            branch = workspace.branch(),
            joined = joined.join(" + "),
            history = if history.is_empty() {
                "(starts at the origin)\n".to_owned()
            } else {
                history
            },
            instructions = super::instructions_section(self.context, Role::Executor),
            response = super::response_section(schema),
        );
        Ok(Prompt::from(text))
    }
}
