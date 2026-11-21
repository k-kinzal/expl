//! Execution — realise a ready step on its own branch.
//!
//! Reacts to `node.ready`. The step is claimed (`node.started`), its branch
//! starts from its predecessors (merged when paths join), the agent works
//! in the step's worktree, and expl commits whatever it left there as the
//! branch tip: `node.completed` (with `node.ready` for successors that can
//! now run) or `node.failed`. A failed run still commits its work so it can
//! be inspected.
//!
//! When joining paths conflicts, the agent first resolves each conflicting
//! merge on its own, and the remaining predecessors are merged after it;
//! the step's work starts only once all of them are in. A conflict left
//! unresolved fails the step, and so does a tip that does not contain
//! every predecessor's commit.
//!
//! Each ready step is its own event, so as many steps run at once as there
//! are execution processes.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agent::AgentError;
use crate::graph::{Graph, Node, NodeId, NodeKind, NodeStatus};
use crate::prompt::Prompt;
use crate::role::{Reaction, Report, RoleError};
use crate::workspace::{Workspace, Workspaces};

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
    /// The agent's summary or the failure.
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
            "succeeded": {"type": "boolean", "description": "Whether the objective is achieved in the working directory."},
            "summary": {"type": "string", "description": "What you changed, how you verified it, and anything left undone."}
        }
    })
}

pub(crate) fn react(reaction: &Reaction<'_>, graph: &mut Graph) -> Result<Report, RoleError> {
    let id = reaction.subject()?;
    let node = graph.node(id)?;
    if node.kind != NodeKind::Step {
        return Ok(Reaction::ignore(format!("{id} is a {} node", node.kind)));
    }
    // A step still `running` on a repeated delivery was abandoned by a
    // process whose lease expired: take it over.
    let reclaim = node.status == NodeStatus::Running && reaction.attempts() > 1;
    if node.status != NodeStatus::Planned && !reclaim {
        return Ok(Reaction::ignore(format!("{id} is {}", node.status)));
    }
    if let Some(blocking) = graph
        .predecessors(id)?
        .into_iter()
        .find(|p| !p.is_realized())
    {
        return Ok(Reaction::ignore(format!(
            "predecessor {} is {}; {id} becomes ready when it completes",
            blocking.id, blocking.status
        )));
    }
    if !graph.claim(id, reclaim)? {
        return Ok(Reaction::ignore(format!(
            "{id} was claimed by another process, or gained a predecessor that is not completed"
        )));
    }
    // Read after the claim: a claimed step can no longer be joined, so these
    // are the predecessors its branch starts from.
    let mut predecessors = graph.predecessors(id)?;
    if predecessors.is_empty() {
        predecessors.push(graph.origin()?);
    }

    let branch = Workspaces::branch_name(id);
    let mut workspace = match reaction.context().workspaces().create(&node, &predecessors) {
        Ok(workspace) => workspace,
        Err(error) => return fail(graph, &node, &branch, None, error.to_string()),
    };
    graph.set_branch(id, &branch, None)?;

    let schema = schema();
    while !workspace.is_joined() {
        let prompt = resolution_prompt(reaction, &node, &workspace, &schema);
        let (resolved, summary) = invoke(reaction, graph, id, &workspace, &prompt, &schema)?;
        if !resolved {
            return fail(
                graph,
                &node,
                &branch,
                None,
                format!("merge conflicts were not resolved: {summary}"),
            );
        }
        if let Err(error) = workspace.resume() {
            return fail(graph, &node, &branch, None, format!("{summary}\n\n{error}"));
        }
    }

    let prompt = prompt(reaction, &node, &schema);
    let (succeeded, summary) = invoke(reaction, graph, id, &workspace, &prompt, &schema)?;
    let message = format!("{}\n\n{}", node.title, summary);
    let head = match workspace.commit(&message) {
        Ok(head) => head,
        Err(error) => return fail(graph, &node, &branch, None, format!("{summary}\n\n{error}")),
    };
    graph.set_branch(id, &branch, Some(&head))?;
    if !succeeded {
        return fail(graph, &node, &branch, Some(head), summary);
    }
    if let Err(error) = workspace.verify_predecessors(&head) {
        return fail(
            graph,
            &node,
            &branch,
            Some(head),
            format!("{summary}\n\n{error}"),
        );
    }
    graph.complete(id, &summary)?;
    Ok(Report::Execution(ExecutionReport {
        node: id,
        status: NodeStatus::Completed,
        branch,
        head_commit: Some(head),
        summary,
    }))
}

/// Invoke the agent once in the step's worktree: whether it reports
/// success, and its summary. An agent that fails or answers malformed
/// counts as reporting failure.
fn invoke(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    id: NodeId,
    workspace: &Workspace,
    prompt: &Prompt,
    schema: &serde_json::Value,
) -> Result<(bool, String), RoleError> {
    let result: Result<Response, RoleError> =
        reaction.invoke(graph, Some(id), workspace.path(), prompt, schema);
    match result {
        Ok(response) => Ok((response.succeeded, response.summary)),
        Err(error @ RoleError::Agent(AgentError::Interrupted)) => {
            // Stopping, not failing: give the step back so the redelivered
            // event can claim it again.
            graph.unclaim(id)?;
            Err(error)
        }
        Err(error @ (RoleError::Agent(_) | RoleError::Response { .. })) => {
            Ok((false, format!("execution agent failed: {error}")))
        }
        Err(error) => {
            graph.fail(id, &error.to_string())?;
            Err(error)
        }
    }
}

fn fail(
    graph: &mut Graph,
    node: &Node,
    branch: &str,
    head_commit: Option<String>,
    summary: String,
) -> Result<Report, RoleError> {
    graph.fail(node.id, &summary)?;
    Ok(Report::Execution(ExecutionReport {
        node: node.id,
        status: NodeStatus::Failed,
        branch: branch.to_owned(),
        head_commit,
        summary,
    }))
}

/// The prompt for resolving one predecessor merge: the conflicted files and
/// the step they prepare for — nothing about the graph, the goals, or other
/// steps.
fn resolution_prompt(
    reaction: &Reaction<'_>,
    node: &Node,
    workspace: &Workspace,
    schema: &serde_json::Value,
) -> Prompt {
    Prompt::from(format!(
        "## Task\n\nCombine earlier work before starting on: {title}\n\n\
         Merging earlier work into the working directory left conflicts in: {conflicts}. \
         Resolve every conflict so that the intent of both sides is kept, and remove every \
         conflict marker. Do only this: do not start on the task above yet, leave your changes \
         uncommitted, and do not conclude or abort the merge.\n\
         {instructions}\nReport `succeeded: false` honestly if a conflict cannot be resolved.\n\
         {response}",
        title = node.title,
        conflicts = workspace.conflicts().join(", "),
        instructions = super::instructions_section(reaction.agent()),
        response = super::response_section(schema),
    ))
}

/// The prompt: the step's own title and objective — nothing about the
/// graph, the goals, or other steps.
fn prompt(reaction: &Reaction<'_>, node: &Node, schema: &serde_json::Value) -> Prompt {
    Prompt::from(format!(
        "## Task\n\n{title}\n\n{objective}\n\n\
         Do the work in the working directory: edit files, run builds and tests, and verify the \
         objective. Leave your changes uncommitted, and do not switch branches.\n\
         {instructions}\nReport `succeeded: false` honestly if the objective is not achieved.\n\
         {response}",
        title = node.title,
        objective = node.objective.trim(),
        instructions = super::instructions_section(reaction.agent()),
        response = super::response_section(schema),
    ))
}
