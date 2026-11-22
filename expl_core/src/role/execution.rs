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
//! An investigation step changes nothing: the agent answers its question in
//! a scratch worktree at the code it is about (its predecessors' work, which
//! must be one line of history), and the step completes with a finding — yes,
//! no or unknown, with evidence — and no branch or commit of its own. Its
//! `head_commit` is that code, so steps after it start from the same place.
//!
//! Each ready step is its own event, so as many steps run at once as there
//! are execution processes.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agent::AgentError;
use crate::event::Event;
use crate::graph::{Answer, Graph, GraphError, Node, NodeId, NodeKind, NodeStatus};
use crate::prompt::Prompt;
use crate::role::{Reaction, Report, RoleError};
use crate::trunk::Trunk;
use crate::workspace::Workspace;

/// What one execution did.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionReport {
    /// The step.
    pub node: NodeId,
    /// `completed` or `failed`.
    pub status: NodeStatus,
    /// Branch holding the work (empty for an investigation, or when no
    /// branch could be made).
    pub branch: String,
    /// What an investigation found.
    pub answer: Option<Answer>,
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

#[derive(Debug, Deserialize)]
struct Finding {
    investigated: bool,
    answer: Answer,
    evidence: String,
}

fn finding_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["investigated", "answer", "evidence"],
        "properties": {
            "investigated": {"type": "boolean", "description": "Whether you could carry out the investigation at all. An open question is still investigated: answer `unknown`."},
            "answer": {"type": "string", "enum": ["yes", "no", "unknown"], "description": "The answer to the question; `unknown` when the evidence does not settle it."},
            "evidence": {"type": "string", "description": "What you checked and what you found, concretely enough that someone else can verify the answer."}
        }
    })
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
    if node.investigation {
        return investigate(reaction, graph, &node, &predecessors);
    }

    // Each attempt gets a branch that did not exist before, under a name no
    // existing ref and no trunk branch takes; the one git created is
    // recorded.
    let context = reaction.context();
    let trunk = Trunk::new(context.project(), context.declaration().trunk.as_deref());
    let trunk = trunk.branches(graph)?;
    let mut workspace = match context.workspaces().create(&node, &predecessors, &trunk) {
        Ok(workspace) => workspace,
        Err(error) => return fail(graph, &node, "", None, error.to_string()),
    };
    let branch = workspace.branch().to_owned();
    graph.set_branch(id, &branch, None)?;
    let result = carry_out(reaction, graph, &node, &branch, &mut workspace);
    // Settled either way: the branch holds what there is, and the worktree
    // goes. Not when the lease was lost or the run interrupted: the step may
    // be another process's now, working in the same place.
    if matches!(result, Ok(Report::Execution(_))) && !reaction.lease_lost() {
        // Best effort: `expl doctor --fix` removes a leftover.
        reaction.context().workspaces().release(workspace).ok();
    }
    result
}

/// Join the claimed change step `node`'s predecessors in `workspace`, have
/// the agent do the step, and commit and settle it.
fn carry_out(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    node: &Node,
    branch: &str,
    workspace: &mut Workspace,
) -> Result<Report, RoleError> {
    let id = node.id;
    let schema = schema();
    while !workspace.is_joined() {
        let prompt = resolution_prompt(reaction, node, workspace, &schema);
        let (resolved, summary) = invoke(reaction, graph, id, workspace, &prompt, &schema)?;
        if !resolved {
            return fail(
                graph,
                node,
                branch,
                None,
                format!("merge conflicts were not resolved: {summary}"),
            );
        }
        if let Err(error) = workspace.resume() {
            return fail(graph, node, branch, None, format!("{summary}\n\n{error}"));
        }
    }

    let prompt = prompt(reaction, node, &schema);
    let (succeeded, summary) = invoke(reaction, graph, id, workspace, &prompt, &schema)?;
    // The lease is the right to write this step. Lost while the agent ran,
    // the step may already be another process's: touch nothing of it.
    if reaction.lease_lost() {
        return Err(RoleError::Agent(AgentError::Interrupted));
    }
    let message = format!("{}\n\n{}", node.title, summary);
    let head = match workspace.commit(&message) {
        Ok(head) => head,
        Err(error) => return fail(graph, node, branch, None, format!("{summary}\n\n{error}")),
    };
    graph.set_branch(id, branch, Some(&head))?;
    if !succeeded {
        return fail(graph, node, branch, Some(head), summary);
    }
    if let Err(error) = workspace.verify_predecessors(&head) {
        return fail(
            graph,
            node,
            branch,
            Some(head),
            format!("{summary}\n\n{error}"),
        );
    }
    if reaction.lease_lost() {
        return Err(RoleError::Agent(AgentError::Interrupted));
    }
    graph.complete(id, &summary)?;
    Ok(Report::Execution(ExecutionReport {
        node: id,
        status: NodeStatus::Completed,
        branch: branch.to_owned(),
        answer: None,
        head_commit: Some(head),
        summary,
    }))
}

/// Answer the question of the claimed investigation `node` at the code its
/// `predecessors` make up, and record the finding.
fn investigate(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    node: &Node,
    predecessors: &[Node],
) -> Result<Report, RoleError> {
    let id = node.id;
    let mut commits: Vec<String> = Vec::new();
    for predecessor in predecessors {
        if let Some(commit) = &predecessor.head_commit
            && !commits.contains(commit)
        {
            commits.push(commit.clone());
        }
    }
    let workspaces = reaction.context().workspaces();
    let code = match workspaces.newest_of(&commits) {
        Ok(Some(code)) => code,
        Ok(None) => {
            return fail(
                graph,
                node,
                "",
                None,
                "an investigation reads one state of the code, but the steps it builds on are on \
                 separate lines of history; combine them in a change step first"
                    .to_owned(),
            );
        }
        Err(error) => return fail(graph, node, "", None, error.to_string()),
    };
    let scratch = match workspaces.scratch_at(id, &code) {
        Ok(scratch) => scratch,
        Err(error) => return fail(graph, node, "", None, error.to_string()),
    };
    let schema = finding_schema();
    let path = graph.ancestors(id)?;
    let prompt = investigation_prompt(reaction, node, &path, &schema);
    // An investigation changes nothing: a run that changed its working
    // directory (or moved HEAD) is no finding about the code it was given.
    let result: Result<Finding, RoleError> =
        reaction.invoke_read_only(graph, Some(id), &scratch, &prompt, &schema);
    drop(scratch);
    // Whatever the agent answered, a lease another process took over means
    // the step is no longer ours to record.
    if reaction.lease_lost() {
        return Err(RoleError::Agent(AgentError::Interrupted));
    }
    let finding = match result {
        Ok(finding) => finding,
        Err(error @ RoleError::Agent(AgentError::Interrupted)) => {
            graph.unclaim(id)?;
            return Err(error);
        }
        Err(error @ RoleError::Agent(_)) => {
            return fail(
                graph,
                node,
                "",
                None,
                format!("investigation agent failed: {error}"),
            );
        }
        Err(error @ RoleError::ScratchModified { .. }) => {
            return fail(
                graph,
                node,
                "",
                None,
                format!("the investigation changed the code it was to investigate: {error}"),
            );
        }
        Err(error) => {
            graph.fail(id, &error.to_string())?;
            return Err(error);
        }
    };
    if !finding.investigated {
        return fail(graph, node, "", None, finding.evidence);
    }
    graph.record_finding(id, &code, finding.answer, &finding.evidence)?;
    Ok(Report::Execution(ExecutionReport {
        node: id,
        status: NodeStatus::Completed,
        branch: String::new(),
        answer: Some(finding.answer),
        head_commit: Some(code),
        summary: finding.evidence,
    }))
}

/// The prompt for an investigation: its question, and what investigations
/// before it on its path found — nothing about other paths or the goals.
fn investigation_prompt(
    reaction: &Reaction<'_>,
    node: &Node,
    path: &[Node],
    schema: &serde_json::Value,
) -> Prompt {
    let earlier = super::digest::findings(path);
    let earlier = if earlier.is_empty() {
        String::new()
    } else {
        format!("\n## Found earlier on this path\n\n{earlier}")
    };
    Prompt::from(format!(
        "## Question\n\n{title}\n\n{objective}\n\n\
         This is an investigation: answer the question, and change nothing. The working \
         directory holds the code the question is about. Read the code, and run builds, tests \
         or commands as you need to gather evidence, but do not edit files, commit, or switch \
         branches: nothing you leave in the working directory is kept.\n\n\
         Answer `yes` or `no` when the evidence settles the question, `unknown` when it does \
         not, and give the evidence — what you checked and what you found — so that someone \
         else can verify the answer. Report `investigated: false` only when you could not carry \
         out the investigation at all.\n\
         {earlier}{instructions}{response}",
        title = node.title,
        objective = node.objective,
        instructions = super::instructions_section(reaction.agent()),
        response = super::response_section(schema),
    ))
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
        reaction.invoke(graph, Some(id), workspace, prompt, schema);
    match result {
        Ok(response) => Ok((response.succeeded, response.summary)),
        Err(error @ RoleError::Agent(AgentError::Interrupted)) => {
            // Stopping, not failing: give the step back so the redelivered
            // event can claim it again — unless the lease was lost, when the
            // step may already belong to the process that took it over.
            if !reaction.lease_lost() {
                graph.unclaim(id)?;
            }
            Err(error)
        }
        Err(error @ RoleError::Agent(_)) => Ok((false, format!("execution agent failed: {error}"))),
        Err(error) => {
            graph.fail(id, &error.to_string())?;
            Err(error)
        }
    }
}

/// The reaction to `event` ended for good: a step it left `running` fails.
pub(crate) fn abandon(
    event: &Event,
    graph: &mut Graph,
    why: &str,
) -> Result<Option<NodeId>, GraphError> {
    let Some(id) = event.node else {
        return Ok(None);
    };
    let node = match graph.node(id) {
        Ok(node) => node,
        Err(GraphError::NodeNotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    if node.kind != NodeKind::Step || node.status != NodeStatus::Running {
        return Ok(None);
    }
    graph.fail(id, why)?;
    Ok(Some(id))
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
        answer: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;
    use crate::role::Role;

    #[test]
    fn a_terminally_failed_execution_leaves_its_step_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut graph = Graph::open(&dir.path().join("expl.db")).expect("graph");
        let origin = graph
            .create_origin("o", Some("main"), "abc")
            .expect("origin");
        let goal = graph.add_goal("g", "g", false, &[]).expect("goal");
        let running = graph
            .propose_step("r", "r", &[origin], &[goal])
            .expect("step");
        let planned = graph
            .propose_step("p", "p", &[origin], &[goal])
            .expect("step");
        graph.plan_step(running, 1).expect("plan");
        graph.plan_step(planned, 1).expect("plan");
        let events = graph.events(None, 100).expect("events");
        let ready = |id| {
            events
                .iter()
                .find(|e| e.kind == EventKind::NodeReady && e.node == Some(id))
                .cloned()
                .expect("node.ready")
        };
        assert!(graph.claim(running, false).expect("claim"));

        // Only execution leaves steps behind.
        assert_eq!(
            Role::Planning
                .abandon(&ready(running), &mut graph, "gone")
                .expect("abandon"),
            None
        );
        assert_eq!(
            graph.node(running).expect("node").status,
            NodeStatus::Running
        );

        assert_eq!(
            Role::Execution
                .abandon(&ready(running), &mut graph, "its holder died")
                .expect("abandon"),
            Some(running)
        );
        assert_eq!(
            graph.node(running).expect("node").status,
            NodeStatus::Failed
        );
        let last = graph.events(None, 100).expect("events");
        let last = last.last().expect("an event");
        assert_eq!(
            (last.kind, last.node),
            (EventKind::NodeFailed, Some(running))
        );

        // A step that is not running, or already failed, is left alone.
        let before = graph.last_event().expect("last");
        for id in [planned, running] {
            assert_eq!(
                Role::Execution
                    .abandon(&ready(id), &mut graph, "again")
                    .expect("abandon"),
                None
            );
        }
        assert_eq!(
            graph.node(planned).expect("node").status,
            NodeStatus::Planned
        );
        assert_eq!(graph.last_event().expect("last"), before);
    }
}
