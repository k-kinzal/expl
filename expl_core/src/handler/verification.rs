//! Verification — test a ready hypothesis: realise a change step on its
//! own branch, or answer an investigation step's question.
//!
//! Reacts to `node.ready`. The step is claimed (`node.started`), its branch
//! starts from its predecessors (merged when paths join), the agent works
//! in the attempt's worktree, and expl commits whatever it left there as the
//! branch tip: `node.completed` (with `node.ready` for successors that can
//! now run) or `node.failed`. A failed run still commits its work so it can
//! be inspected.
//!
//! An attempt records its new branch as soon as it has one, clearing any tip
//! an earlier attempt left, and publishes its tip, outcome and events with
//! the branch in one transaction checked against its lease
//! ([`Graph::publish`]). An attempt that lost the step publishes nothing,
//! so the step never pairs one attempt's branch with another's tip.
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
//! A step evaluation decided to **continue** ([`Decision::Continue`]) runs
//! again with what evaluation found — how far the last attempt got, why it
//! stopped, what remains — in its prompt; a change step's new branch starts
//! from the commit the last attempt left, so the work carries on rather
//! than starting over.
//!
//! Each ready step is its own event, so as many steps run at once as there
//! are verification processes.

use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agent::AgentError;
use crate::event::{Delivery, LeaseToken};
use crate::graph::{
    Answer, ChangeResult, Conclusion, Decision, Graph, GraphError, Node, NodeId, NodeKind,
    NodeStatus,
};
use crate::handler::{Abandonment, HandlerError, Reaction, Report};
use crate::prompt::Prompt;
use crate::trunk::Trunk;
use crate::workspace::{AgentClone, Workspace, WorkspaceError};

/// What one verification did.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
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

pub(crate) fn react(reaction: &Reaction<'_>, graph: &mut Graph) -> Result<Report, HandlerError> {
    let id = reaction.subject()?;
    let node = graph.node(id)?;
    crate::telemetry::subject(&node);
    if node.kind != NodeKind::Step {
        return Ok(Reaction::ignore(format!("{id} is a {} node", node.kind)));
    }
    if !matches!(node.status, NodeStatus::Planned | NodeStatus::Running) {
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
    if !graph.claim(id, reaction.token())? {
        return Ok(Reaction::ignore(format!(
            "{id} was claimed by another process, or gained a predecessor that is not completed"
        )));
    }
    // A step settled elsewhere meanwhile (#1775) refused this attempt's
    // settlement and kept what it holds: there is nothing left to do, so
    // the event is acknowledged, not retried.
    match realise(reaction, graph, &node) {
        Err(HandlerError::Graph(error)) if error.is_settled(id) => Ok(Reaction::ignore(format!(
            "{id} was settled elsewhere: {error}"
        ))),
        result => result,
    }
}

/// Realise the step `node` this reaction has just claimed: investigate it,
/// or carry its change out on a branch of its own.
fn realise(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    node: &Node,
) -> Result<Report, HandlerError> {
    let id = node.id;
    let token = reaction.token();
    // Read after the claim: a claimed step can no longer be joined, so these
    // are the predecessors its branch starts from.
    let mut predecessors = graph.predecessors(id)?;
    if predecessors.is_empty() {
        predecessors.push(graph.origin()?);
    }
    let continued = continuations(graph, id)?;
    crate::telemetry::attributes([
        KeyValue::new(
            "expl.verification.predecessor_ids",
            crate::telemetry::node_ids(predecessors.iter().map(|node| node.id)),
        ),
        KeyValue::new("expl.verification.continuations", continued.len() as i64),
    ]);
    if node.investigation {
        return investigate(reaction, graph, node, &predecessors, &continued);
    }

    // Each attempt gets its own path and a fresh branch, and touches no
    // other attempt's worktree: a process left behind by an earlier attempt
    // still has its old path, and the attempt that took the step over may
    // already be working in its own.
    let context = reaction.context();
    let workspaces = context.workspaces();
    let trunk = Trunk::new(context.project(), context.declaration().trunk.as_deref());
    let trunk = trunk.branches(graph)?;
    pause(Window::Allocate);
    // Carried on from the newest work an earlier attempt left — an attempt
    // that failed before committing left none, and its conclusion no
    // commit: its commit first, then the predecessors (already in it,
    // unless they moved since).
    let resumed = continued
        .iter()
        .rev()
        .find_map(|c| c.resume_commit.clone())
        .and_then(|commit| {
            let mut left = node.clone();
            left.head_commit = Some(commit);
            let mut start = vec![left];
            start.extend(predecessors.iter().cloned());
            match workspaces.create(node, token, &start, &trunk) {
                Ok(workspace) => Some(workspace),
                Err(WorkspaceError::AttemptExists(_)) => None,
                Err(_) => {
                    // A failed join can leave this attempt's path allocated.
                    // Clear it if clean so the fallback can use the same path.
                    workspaces.unregister(&workspaces.path_of(id, token)).ok();
                    None
                }
            }
        });
    let carried_on = resumed.is_some();
    let created = match resumed {
        Some(workspace) => Ok(workspace),
        None => workspaces.create(node, token, &predecessors, &trunk),
    };
    let mut workspace = match created {
        Ok(workspace) => workspace,
        Err(error) => return fail(graph, token, node, "", None, error.to_string()),
    };
    let branch = workspace.branch().to_owned();
    let attempt = Attempt {
        continued: &continued,
        carried_on,
    };
    let result = graph
        .set_branch(id, token, &branch)
        .map_err(HandlerError::from)
        .and_then(|()| carry_out(reaction, graph, node, &attempt, &branch, &mut workspace));
    // This attempt releases its own worktree, and only that; both are best
    // effort, and `expl doctor --fix` removes a leftover once the step
    // settles. Settled: the branch holds what there is, and the worktree
    // goes. On lease loss or interruption, the worktree goes only if
    // nothing was left in it, so unsaved work stays to be inspected.
    if matches!(result, Ok(Report::Verification(_))) && !reaction.lease_lost() {
        workspaces.release(workspace).ok();
    } else {
        workspaces.unregister(workspace.path()).ok();
    }
    result
}

/// Join the claimed change step `node`'s predecessors in `workspace`, have
/// the agent do the step, and commit and settle it.
fn carry_out(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    node: &Node,
    attempt: &Attempt<'_>,
    branch: &str,
    workspace: &mut Workspace,
) -> Result<Report, HandlerError> {
    let id = node.id;
    let token = reaction.token();
    let schema = schema();
    let workspaces = reaction.context().workspaces();
    // The agent works in a clone of its own; the worktree stays expl's,
    // and takes in what the agent left once it is done.
    while !workspace.is_joined() {
        let clone = match workspaces.clone_for(workspace) {
            Ok(clone) => clone,
            Err(error) => return fail(graph, token, node, branch, None, error.to_string()),
        };
        let prompt = resolution_prompt(reaction, node, workspace, &schema);
        let _scope = crate::telemetry::span(
            "expl.verification.resolve_merge",
            vec![KeyValue::new(
                "expl.workspace.conflict_count",
                workspace.conflicts().len() as i64,
            )],
        )
        .attach();
        let (resolved, summary) = invoke(reaction, graph, id, &clone, &prompt, &schema)?;
        if !resolved {
            return fail(
                graph,
                token,
                node,
                branch,
                None,
                format!("merge conflicts were not resolved: {summary}"),
            );
        }
        if let Err(error) = workspace
            .take_resolution(&clone)
            .and_then(|()| workspace.resume())
        {
            return fail(
                graph,
                token,
                node,
                branch,
                None,
                format!("{summary}\n\n{error}"),
            );
        }
    }

    let clone = match workspaces.clone_for(workspace) {
        Ok(clone) => clone,
        Err(error) => return fail(graph, token, node, branch, None, error.to_string()),
    };
    let frozen = &reaction.context().declaration().workspace.frozen;
    let prompt = prompt(reaction, node, attempt, &schema);
    let (succeeded, summary) = match invoke(reaction, graph, id, &clone, &prompt, &schema) {
        Ok(outcome) => outcome,
        Err(error) => {
            // Stopped part-way: what the agent left stays in this
            // attempt's worktree, uncommitted, to be inspected.
            workspace.take_in(&clone, frozen).ok();
            return Err(error);
        }
    };
    // The lease is the right to write this step. Lost while the agent ran,
    // the step may already be another process's: touch nothing of it
    // beyond this attempt's own worktree.
    if reaction.lease_lost() {
        workspace.take_in(&clone, frozen).ok();
        return Err(HandlerError::Agent(AgentError::Interrupted));
    }
    if let Err(error) = workspace.take_in(&clone, frozen) {
        return fail(
            graph,
            token,
            node,
            branch,
            None,
            format!("{summary}\n\n{error}"),
        );
    }
    drop(clone);
    pause(Window::Commit);
    let message = format!("{}\n\n{}", node.title, summary);
    let committed = crate::telemetry::operation(
        "expl.workspace.commit",
        vec![
            KeyValue::new("expl.node.id", id.0),
            KeyValue::new("expl.workspace.branch", branch.to_owned()),
        ],
        "workspace.commit",
        || workspace.commit(&message),
    );
    let head = match committed {
        Ok(head) => head,
        Err(error) => {
            return fail(
                graph,
                token,
                node,
                branch,
                None,
                format!("{summary}\n\n{error}"),
            );
        }
    };
    // Committed, but nothing of the commit is in the graph yet: the branch,
    // its tip, the outcome and the events are published together below, and
    // only while this delivery still holds the step.
    pause(Window::Publish);
    if reaction.lease_lost() {
        return Err(HandlerError::Agent(AgentError::Interrupted));
    }
    let context = reaction.context();
    if let Some(remote) = &context.declaration().workspace.remote {
        let git = crate::git::Git::in_worktree(context.project().root());
        let reference = format!("refs/heads/{branch}");
        git.run(&["remote", "get-url", "--", remote])
            .map_err(WorkspaceError::from)?;
        git.network(
            &[
                "push",
                "--no-follow-tags",
                &format!("--force-with-lease={reference}:"),
                "--",
                remote,
                &format!("{head}:{reference}"),
            ],
            reaction.interrupt(),
        )
        .map_err(WorkspaceError::from)?;
    }
    if !succeeded {
        return fail(graph, token, node, branch, Some(head), summary);
    }
    if let Err(error) = workspace.verify_predecessors(&head) {
        return fail(
            graph,
            token,
            node,
            branch,
            Some(head),
            format!("{summary}\n\n{error}"),
        );
    }
    if reaction.lease_lost() {
        return Err(HandlerError::Agent(AgentError::Interrupted));
    }
    graph.publish(
        id,
        token,
        &ChangeResult {
            branch,
            head_commit: Some(&head),
            completed: true,
            outcome: &summary,
        },
    )?;
    Ok(Report::Verification(VerificationReport {
        node: id,
        status: NodeStatus::Completed,
        branch: branch.to_owned(),
        answer: None,
        head_commit: Some(head),
        summary,
    }))
}

/// A point in verification between a lease check and a write:
/// the check is not atomic with the write, so another process can take the
/// lease over in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// After the claim, before [`Workspaces::create`](crate::workspace::Workspaces::create)
    /// allocates the attempt's worktree and branch.
    Allocate,
    /// After the check that follows the agent's run, before
    /// [`Workspace::commit`] and the writes after it.
    Commit,
    /// After [`Workspace::commit`], before [`Graph::publish`] records the
    /// commit with the step's outcome.
    Publish,
    /// After the investigation's lease check, before recording its finding.
    Finding,
}

/// What a test runs at a [`Window`].
#[cfg(test)]
type PauseHook = Box<dyn FnMut(Window)>;

#[cfg(test)]
thread_local! {
    /// What [`pause`] runs on this thread (tests only).
    static PAUSE: std::cell::RefCell<Option<PauseHook>> = const { std::cell::RefCell::new(None) };
}

/// Reach `window`: tests can act there (take the lease over, say) before
/// verification goes on; otherwise nothing happens.
fn pause(window: Window) {
    #[cfg(test)]
    PAUSE.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook(window);
        }
    });
    #[cfg(not(test))]
    let _ = window;
}

/// Answer the question of the claimed investigation `node` at the code its
/// `predecessors` make up, and record the finding.
fn investigate(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    node: &Node,
    predecessors: &[Node],
    continued: &[Conclusion],
) -> Result<Report, HandlerError> {
    let id = node.id;
    let token = reaction.token();
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
                token,
                node,
                "",
                None,
                "an investigation reads one state of the code, but the steps it builds on are on \
                 separate lines of history; combine them in a change step first"
                    .to_owned(),
            );
        }
        Err(error) => return fail(graph, token, node, "", None, error.to_string()),
    };
    let scratch = match workspaces.scratch_at(id, &code) {
        Ok(scratch) => scratch,
        Err(error) => return fail(graph, token, node, "", None, error.to_string()),
    };
    let schema = finding_schema();
    let path = graph.ancestors(id)?;
    let prompt = investigation_prompt(reaction, node, &path, continued, &schema);
    // An investigation changes nothing: a run that changed its working
    // directory (or moved HEAD) is no finding about the code it was given.
    let result: Result<Finding, HandlerError> =
        reaction.invoke_read_only(graph, Some(id), &scratch, &prompt, &schema);
    drop(scratch);
    // Whatever the agent answered, a lease another process took over means
    // the step is no longer ours to record.
    if reaction.lease_lost() {
        return Err(HandlerError::Agent(AgentError::Interrupted));
    }
    let finding = match result {
        Ok(finding) => finding,
        Err(error @ HandlerError::Agent(AgentError::Interrupted)) => {
            graph.unclaim(id, token)?;
            return Err(error);
        }
        Err(error @ HandlerError::Agent(AgentError::Transport(_))) => return Err(error),
        Err(error @ HandlerError::Agent(_)) => {
            return fail(
                graph,
                token,
                node,
                "",
                None,
                format!("investigation agent failed: {error}"),
            );
        }
        Err(error @ HandlerError::ScratchModified { .. }) => {
            return fail(
                graph,
                token,
                node,
                "",
                None,
                format!("the investigation changed the code it was to investigate: {error}"),
            );
        }
        Err(error) => {
            graph.fail(id, token, &error.to_string())?;
            return Err(error);
        }
    };
    if !finding.investigated {
        return fail(graph, token, node, "", None, finding.evidence);
    }
    pause(Window::Finding);
    graph.record_finding(id, token, &code, finding.answer, &finding.evidence)?;
    Ok(Report::Verification(VerificationReport {
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
    continued: &[Conclusion],
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
         {earlier}{continued}{instructions}{response}",
        title = node.title,
        objective = node.objective,
        continued = continuation_section(continued, false),
        instructions = super::instructions_section(reaction.consumer()),
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
    clone: &AgentClone,
    prompt: &Prompt,
    schema: &serde_json::Value,
) -> Result<(bool, String), HandlerError> {
    let result: Result<Response, HandlerError> =
        reaction.invoke(graph, Some(id), clone, prompt, schema);
    match result {
        Ok(response) => Ok((response.succeeded, response.summary)),
        Err(error @ HandlerError::Agent(AgentError::Interrupted)) => {
            // Stopping, not failing: give the step back so the redelivered
            // event can claim it again — unless the lease was lost, when the
            // step may already belong to the process that took it over.
            if !reaction.lease_lost() {
                graph.unclaim(id, reaction.token())?;
            }
            Err(error)
        }
        Err(error @ HandlerError::Agent(AgentError::Transport(_))) => Err(error),
        Err(error @ HandlerError::Agent(_)) => {
            Ok((false, format!("verification agent failed: {error}")))
        }
        Err(error) => {
            graph.fail(id, reaction.token(), &error.to_string())?;
            Err(error)
        }
    }
}

/// The reaction to `delivery` ended for good: a step it still holds fails.
/// A step settled elsewhere already is left as it is.
pub(crate) fn abandon(
    delivery: &Delivery,
    graph: &mut Graph,
    why: &str,
) -> Result<Abandonment, GraphError> {
    let Some(id) = delivery.event.node else {
        return Ok(Abandonment::Nothing);
    };
    match graph.fail(id, &delivery.token, why) {
        Ok(()) => Ok(Abandonment::Failed(id)),
        Err(error) if error.is_settled(id) => Ok(Abandonment::Settled(id)),
        Err(GraphError::LeaseLost(_)) => Ok(Abandonment::Nothing),
        Err(error) => Err(error),
    }
}

/// Fail the claimed `node`. A change step that has its `branch` publishes
/// it with `head_commit` (`None` when nothing was committed) and the
/// failure in one transaction; otherwise only the failure is recorded.
fn fail(
    graph: &mut Graph,
    token: &LeaseToken,
    node: &Node,
    branch: &str,
    head_commit: Option<String>,
    summary: String,
) -> Result<Report, HandlerError> {
    if node.investigation || branch.is_empty() {
        graph.fail(node.id, token, &summary)?;
    } else {
        graph.publish(
            node.id,
            token,
            &ChangeResult {
                branch,
                head_commit: head_commit.as_deref(),
                completed: false,
                outcome: &summary,
            },
        )?;
    }
    Ok(Report::Verification(VerificationReport {
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
         conflict marker. Do only this: the task above comes after. The conflicted files as the \
         working directory holds them when you report are taken as the resolution.\n\
         {instructions}\nReport `succeeded: false` honestly if a conflict cannot be resolved.\n\
         {response}",
        title = node.title,
        conflicts = workspace.conflicts().join(", "),
        instructions = super::instructions_section(reaction.consumer()),
        response = super::response_section(schema),
    ))
}

/// The prompt: the step's own title and objective — nothing about the
/// graph, the goals, or other steps.
/// The conclusions that put this step back to run again, oldest first,
/// while it is being carried on: since the last one, nothing concluded it
/// done or abandoned (a continue not put back — another evaluation agent's,
/// say — does not end it). Empty for a step not being continued.
fn continuations(graph: &Graph, id: NodeId) -> Result<Vec<Conclusion>, HandlerError> {
    let concluded = graph.conclusions_of(id)?;
    let reopened = |c: &Conclusion| c.decision == Decision::Continue && c.reopened;
    let Some(last) = concluded.iter().rposition(reopened) else {
        return Ok(Vec::new());
    };
    if concluded[last..]
        .iter()
        .any(|c| c.decision != Decision::Continue)
    {
        return Ok(Vec::new());
    }
    Ok(concluded.into_iter().filter(reopened).collect())
}

/// What earlier attempts of a continued step got done and what evaluation
/// said remains; empty for a first attempt.
fn continuation_section(continued: &[Conclusion], change: bool) -> String {
    if continued.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n## Earlier attempts\n\n\
         This step was attempted before and stopped short. Evaluation checked each attempt and \
         decided to carry on (oldest first):\n\n",
    );
    for (n, conclusion) in continued.iter().enumerate() {
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "{n}. How far it got: {finding}\n   What remains: {remains}\n",
                n = n + 1,
                finding = conclusion.finding.trim(),
                remains = conclusion.consequence.trim(),
            ),
        );
    }
    if change {
        out.push_str(
            "\nThe working directory already holds what the last attempt left, committed: carry \
             on from it rather than starting over, and finish what remains.\n",
        );
    } else {
        out.push_str("\nTake what remains into account in this attempt.\n");
    }
    out
}

/// What a change step's attempt carries on from.
struct Attempt<'a> {
    /// The conclusions that put it back to run ([`continuations`]).
    continued: &'a [Conclusion],
    /// Whether its worktree starts from what the last attempt left.
    carried_on: bool,
}

fn prompt(
    reaction: &Reaction<'_>,
    node: &Node,
    attempt: &Attempt<'_>,
    schema: &serde_json::Value,
) -> Prompt {
    Prompt::from(format!(
        "## Task\n\n{title}\n\n{objective}\n\n\
         Do the work in the working directory: edit files, run builds and tests, and verify the \
         objective. The repository is yours to use as you see fit, git included: what the \
         working directory holds when you report is taken as the result.\n\
         {continued}{instructions}\nReport `succeeded: false` honestly if the objective is not \
         achieved.\n\
         {response}",
        title = node.title,
        objective = node.objective.trim(),
        continued = continuation_section(attempt.continued, attempt.carried_on),
        instructions = super::instructions_section(reaction.consumer()),
        response = super::response_section(schema),
    ))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::rc::Rc;

    use super::*;
    use crate::declaration::Declaration;
    use crate::event::{Delivery, EventId, EventKind, Inbox};
    use crate::git::Git;
    use crate::handler::{Context, Handler};
    use crate::process::{Interrupt, MAX_ATTEMPTS, ProcessId, Processes};
    use crate::project::Project;
    use crate::store::{self, EPOCH};
    use crate::workspace::Workspaces;

    /// A step leased by its first holder, ready to be carried out.
    struct Held {
        _dir: tempfile::TempDir,
        db: PathBuf,
        context: Context,
        graph: Graph,
        step: NodeId,
        holder: ProcessId,
        taker: ProcessId,
        delivery: Delivery,
        interrupt: Interrupt,
    }

    fn held_step(investigation: bool) -> Held {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = Git::new(dir.path());
        git.run(&["init", "-q", "-b", "main"]).expect("init");
        git.run(&[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "Start",
        ])
        .expect("commit");
        let project = Project::at(dir.path());
        std::fs::create_dir(project.state_dir()).expect("state directory");
        let db = project.database_path();
        let mut graph = Graph::open(&db).expect("graph");
        let origin = graph
            .create_origin(
                "Start",
                Some("main"),
                &git.resolve_commit("HEAD").expect("head"),
            )
            .expect("origin");
        let goal = graph.add_goal("g", "g", false, &[]).expect("goal");
        let step = graph
            .propose(&crate::graph::NewStep {
                investigation,
                title: "s",
                objective: "s",
                after: &[origin],
                before: &[goal],
                ..Default::default()
            })
            .expect("step");
        graph.plan_step(step, 1).expect("plan"); // node.ready
        let declaration = Declaration::parse(
            r#"
            [consumer.verifier]
            on = ["node.ready"]
            handler = "verification"
            driver = "generic"
            command = ["sh", "-c", '''if grep '^## Question$' >/dev/null; then printf '%s\n' "$1"; else echo old > old.txt; printf '%s\n' "$2"; fi''', "agent", '{"investigated":true,"answer":"yes","evidence":"old holder"}', '{"succeeded":true,"summary":"old holder"}']
            replicas = 1

            [consumer.replacement]
            on = ["node.ready"]
            handler = "verification"
            driver = "generic"
            command = ["false"]
            replicas = 1
            "#,
        )
        .expect("declaration");
        let context = Context::new(project.clone(), declaration);
        let mut processes = Processes::open(&db, &project.logs_dir()).expect("processes");
        let holder = processes
            .create("verifier", None, false)
            .expect("process")
            .id;
        let taker = processes
            .create("replacement", None, false)
            .expect("process")
            .id;
        let delivery = Inbox::open(&db, "verifier")
            .expect("inbox")
            .lease(Handler::Verification.events(), holder, false, MAX_ATTEMPTS)
            .expect("lease")
            .expect("node.ready");
        Held {
            _dir: dir,
            db,
            context,
            graph,
            step,
            holder,
            taker,
            delivery,
            interrupt: Interrupt::default().for_reaction(),
        }
    }

    /// What the process that took the step over recorded of it.
    #[derive(Debug)]
    struct Takeover {
        /// `node.started` of the takeover: events after it are not the old
        /// holder's to record.
        started: EventId,
        /// The step as the takeover left it.
        node: Node,
        /// The new holder's workspace, for a change step.
        workspace: Option<Workspace>,
    }

    /// Runs a [`pause`] hook on this thread for as long as it lives.
    struct Hooked;

    impl Hooked {
        fn install(hook: impl FnMut(Window) + 'static) -> Self {
            PAUSE.with_borrow_mut(|slot| *slot = Some(Box::new(hook)));
            Self
        }
    }

    impl Drop for Hooked {
        fn drop(&mut self) {
            PAUSE.with_borrow_mut(|slot| *slot = None);
        }
    }

    /// At `at`, another agent receives the event. It can take over the
    /// step only after the first agent's lease expires.
    fn take_over_at(held: &Held, at: Window) -> (Hooked, Rc<RefCell<Option<Takeover>>>) {
        let takeover = Rc::new(RefCell::new(None));
        let record = Rc::clone(&takeover);
        let (db, context, step, taker) =
            (held.db.clone(), held.context.clone(), held.step, held.taker);
        let first = held.delivery.clone();
        let hook = Hooked::install(move |window| {
            if window != at {
                return;
            }
            let delivery = Inbox::open(&db, "replacement")
                .expect("inbox")
                .lease(Handler::Verification.events(), taker, false, MAX_ATTEMPTS)
                .expect("lease")
                .expect("replacement agent's delivery");
            assert_eq!(delivery.attempts, 1);
            assert_eq!(delivery.event.id, first.event.id);
            let mut graph = Graph::open(&db).expect("graph");
            let started = graph.last_event().expect("last");
            let agent = context.consumer("replacement").expect("agent");
            let interrupt = Interrupt::default().for_reaction();
            let reaction = Reaction::new(&context, &agent, taker, &delivery, &interrupt);
            assert!(matches!(
                react(&reaction, &mut graph).expect("reaction while holder is live"),
                Report::Ignored { .. }
            ));
            assert_eq!(graph.last_event().expect("last"), started);
            store::open(&db)
                .expect("db")
                .execute(
                    &format!(
                        "UPDATE deliveries SET leased_until = {EPOCH} - 1 WHERE agent = 'verifier'"
                    ),
                    [],
                )
                .expect("expire first holder's delivery");
            assert!(graph.claim(step, &delivery.token).expect("reclaim"));
            // At Commit, leave the old checkout in place so its commit
            // reaches the fenced `set_branch` call. Otherwise the new holder
            // works in a worktree of its own, with a file not yet committed.
            let workspace = if matches!(at, Window::Allocate | Window::Publish) {
                let node = graph.node(step).expect("step");
                let predecessors = vec![graph.origin().expect("origin")];
                let trunk = Trunk::new(context.project(), context.declaration().trunk.as_deref())
                    .branches(&graph)
                    .expect("trunk");
                let workspace = context
                    .workspaces()
                    .create(&node, &delivery.token, &predecessors, &trunk)
                    .expect("the new holder's workspace");
                graph
                    .set_branch(step, &delivery.token, workspace.branch())
                    .expect("branch");
                std::fs::write(workspace.path().join("replacement.txt"), "replacement\n")
                    .expect("the new holder's work");
                Some(workspace)
            } else {
                None
            };
            // The first agent can renew its own inbox lease even though
            // that lease no longer holds the step.
            Inbox::open(&db, "verifier")
                .expect("inbox")
                .renew(&first)
                .expect("first agent still holds its inbox lease");
            assert!(
                !graph
                    .claim(step, &first.token)
                    .expect("old holder cannot retake step")
            );
            *record.borrow_mut() = Some(Takeover {
                started: graph.last_event().expect("last").expect("takeover"),
                node: graph.node(step).expect("step"),
                workspace,
            });
        });
        (hook, takeover)
    }

    fn assert_takeover_at(at: Window) {
        let mut held = held_step(at == Window::Finding);
        let (_hook, takeover) = take_over_at(&held, at);
        let agent = held.context.consumer("verifier").expect("agent");
        let reaction = Reaction::new(
            &held.context,
            &agent,
            held.holder,
            &held.delivery,
            &held.interrupt,
        );
        assert!(matches!(
            react(&reaction, &mut held.graph),
            Err(HandlerError::Graph(GraphError::LeaseLost(id))) if id == held.step
        ));
        let takeover = takeover.borrow();
        let takeover = takeover.as_ref().expect("the step was taken over");
        assert!(!reaction.lease_lost(), "the inbox lease was never lost");
        assert!(matches!(
            held.graph.fail(held.step, &held.delivery.token, "late failure"),
            Err(GraphError::LeaseLost(id)) if id == held.step
        ));
        assert!(matches!(
            held.graph.unclaim(held.step, &held.delivery.token),
            Err(GraphError::LeaseLost(id)) if id == held.step
        ));
        assert_eq!(
            Handler::Verification
                .abandon(&held.delivery, &mut held.graph, "late abandonment")
                .expect("abandon"),
            Abandonment::Nothing
        );
        assert_eq!(held.graph.node(held.step).expect("step"), takeover.node);
        let events = held.graph.events(None, 100).expect("events");
        let starts: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::NodeStarted && e.node == Some(held.step))
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[1].id, takeover.started);
        assert_eq!(events.last().expect("last").id, takeover.started);
        if let Some(workspace) = &takeover.workspace {
            workspace.check_branch().expect("new holder's checkout");
        }
        if at == Window::Commit {
            let git = Git::new(held.context.project().root());
            let branch = takeover
                .node
                .branch
                .as_deref()
                .expect("old holder's branch");
            assert_ne!(
                git.resolve_commit(branch).expect("late commit"),
                git.resolve_commit("main").expect("base")
            );
        }
    }

    #[test]
    fn a_verification_left_alone_passes_every_window_and_completes() {
        let mut held = held_step(false);
        let reached = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&reached);
        let _hook = Hooked::install(move |window| seen.borrow_mut().push(window));
        let agent = held.context.consumer("verifier").expect("agent");
        let reaction = Reaction::new(
            &held.context,
            &agent,
            held.holder,
            &held.delivery,
            &held.interrupt,
        );
        let report = react(&reaction, &mut held.graph).expect("reaction");
        assert!(matches!(
            report,
            Report::Verification(VerificationReport {
                status: NodeStatus::Completed,
                ..
            })
        ));
        assert_eq!(
            *reached.borrow(),
            vec![Window::Allocate, Window::Commit, Window::Publish]
        );
        assert_eq!(
            held.graph.node(held.step).expect("step").status,
            NodeStatus::Completed
        );
    }

    #[test]
    fn a_stale_process_cannot_write_into_the_reclaiming_attempts_commit() {
        let mut held = held_step(false);
        let retry = Processes::open(&held.db, &held.context.project().logs_dir())
            .expect("processes")
            .create("verifier", None, false)
            .expect("retry process")
            .id;
        assert!(
            held.graph
                .claim(held.step, &held.delivery.token)
                .expect("claim")
        );
        let step = held.graph.node(held.step).expect("step");
        let origin = held.graph.origin().expect("origin");
        let workspaces = held.context.workspaces();
        let first = workspaces
            .create(&step, &held.delivery.token, &[origin], &[])
            .expect("first worktree");
        let old_path = first.path().to_owned();
        held.graph
            .set_branch(held.step, &held.delivery.token, first.branch())
            .expect("record first branch");
        let mut stale = Some(
            Command::new("sh")
                .args([
                    "-ec",
                    r#"read -r signal; mkdir -p "$1"; echo stale > "$1/stale.txt""#,
                    "agent",
                ])
                .arg(&old_path)
                .current_dir(&old_path)
                .stdin(Stdio::piped())
                .spawn()
                .expect("start a process in the first worktree"),
        );
        store::open(&held.db)
            .expect("db")
            .execute(
                &format!("UPDATE deliveries SET leased_until = {EPOCH} - 1"),
                [],
            )
            .expect("expire");
        let delivery = Inbox::open(&held.db, "verifier")
            .expect("inbox")
            .lease(Handler::Verification.events(), retry, false, MAX_ATTEMPTS)
            .expect("lease")
            .expect("redelivery");
        assert_eq!(delivery.attempts, 2);
        let _hook = Hooked::install(move |window| {
            if window == Window::Commit {
                let mut stale = stale.take().expect("stale process");
                stale
                    .stdin
                    .take()
                    .expect("stale process input")
                    .write_all(b"write now\n")
                    .expect("signal the stale process");
                assert!(stale.wait().expect("stale process exit").success());
            }
        });
        let agent = held.context.consumer("verifier").expect("agent");
        let reaction = Reaction::new(&held.context, &agent, retry, &delivery, &held.interrupt);
        let report = react(&reaction, &mut held.graph).expect("reclaim and verify");
        let Report::Verification(report) = report else {
            panic!("expected verification, got {report:?}");
        };
        assert_eq!(report.status, NodeStatus::Completed);
        let commit = report.head_commit.expect("commit");
        let files = Git::new(held.context.project().root())
            .run(&["ls-tree", "--name-only", &commit])
            .expect("list committed files");
        assert_eq!(
            files, "old.txt",
            "only the reclaiming agent's file is committed"
        );
        assert!(
            old_path.join("stale.txt").is_file(),
            "the stale write succeeded"
        );
        // The reclaiming attempt released its own worktree and left the
        // stale one's alone.
        let registered = workspaces.registered().expect("registrations");
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].0, held.step);
        assert_eq!(registered[0].1.file_name(), old_path.file_name());
        workspaces.discard(held.step).expect("discard leftovers");
        assert!(!old_path.exists());
    }

    #[test]
    fn verification_commits_without_resolving_a_dangling_symbolic_trunk() {
        let mut held = held_step(false);
        let mut declaration = held.context.declaration().clone();
        declaration.trunk = Some("trunk".to_owned());
        held.context = Context::new(held.context.project().clone(), declaration);
        let git = Git::new(held.context.project().root());
        let trunk = "refs/heads/trunk";
        let target = format!("refs/heads/{}", Workspaces::branch_name(held.step));
        git.run(&["symbolic-ref", trunk, &target])
            .expect("trunk ref");
        assert!(git.resolve_commit(trunk).is_err());

        let agent = held.context.consumer("verifier").expect("agent");
        let reaction = Reaction::new(
            &held.context,
            &agent,
            held.holder,
            &held.delivery,
            &held.interrupt,
        );
        let Report::Verification(report) = react(&reaction, &mut held.graph).expect("reaction")
        else {
            panic!("expected a verification report");
        };

        assert_eq!(report.status, NodeStatus::Completed, "{}", report.summary);
        let commit = report.head_commit.expect("committed step");
        assert_eq!(
            git.run(&["show", &format!("{commit}:old.txt")])
                .expect("committed file"),
            "old"
        );
        assert_eq!(
            git.run(&["symbolic-ref", trunk]).expect("trunk target"),
            target
        );
        assert!(
            git.resolve_commit(trunk).is_err(),
            "trunk is still dangling"
        );
        assert_eq!(report.branch, Workspaces::branch_candidate(held.step, 2));
        assert_eq!(
            git.resolve_commit(&format!("refs/heads/{}", report.branch))
                .expect("step branch tip"),
            commit
        );
    }

    #[test]
    fn a_takeover_before_allocation_leaves_the_step_to_the_new_holder() {
        assert_takeover_at(Window::Allocate);
    }

    #[test]
    fn a_stale_verifier_resumed_before_allocation_leaves_the_replacement_worktree_intact() {
        let mut held = held_step(false);
        let (_hook, takeover) = take_over_at(&held, Window::Allocate);
        let agent = held.context.consumer("verifier").expect("agent");
        let reaction = Reaction::new(
            &held.context,
            &agent,
            held.holder,
            &held.delivery,
            &held.interrupt,
        );
        assert!(matches!(
            react(&reaction, &mut held.graph),
            Err(HandlerError::Graph(GraphError::LeaseLost(id))) if id == held.step
        ));
        let takeover = takeover.borrow();
        let replacement = takeover
            .as_ref()
            .expect("the step was taken over")
            .workspace
            .as_ref()
            .expect("the replacement's worktree");
        let workspaces = held.context.workspaces();

        let registered = workspaces.registered().expect("registrations");
        assert_eq!(
            registered
                .iter()
                .map(|(node, path)| (*node, path.file_name().map(ToOwned::to_owned)))
                .collect::<Vec<_>>(),
            vec![(
                held.step,
                replacement.path().file_name().map(ToOwned::to_owned)
            )],
            "only the replacement's registration is left"
        );
        replacement
            .check_branch()
            .expect("the replacement's checkout is on its branch");
        assert_eq!(
            std::fs::read_to_string(replacement.path().join("replacement.txt"))
                .expect("the replacement's file"),
            "replacement\n"
        );
        assert_eq!(
            held.graph.node(held.step).expect("step").branch.as_deref(),
            Some(replacement.branch())
        );
        let stale = workspaces.path_of(held.step, &held.delivery.token);
        assert!(
            !stale.exists(),
            "the stale attempt released its own, clean worktree"
        );
    }

    #[test]
    fn a_takeover_before_the_commit_leaves_the_step_to_the_new_holder() {
        assert_takeover_at(Window::Commit);
    }

    #[test]
    fn a_takeover_before_publication_leaves_the_step_to_the_new_holder() {
        assert_takeover_at(Window::Publish);
    }

    #[test]
    fn a_takeover_before_the_finding_leaves_the_step_to_the_new_holder() {
        assert_takeover_at(Window::Finding);
    }

    #[test]
    fn a_finding_recorded_elsewhere_first_is_kept_and_the_late_one_ignored() {
        let mut held = held_step(true);
        let (db, step, taker) = (held.db.clone(), held.step, held.taker);
        let _hook = Hooked::install(move |window| {
            if window != Window::Finding {
                return;
            }
            // Another agent takes the step over once the first holder's
            // lease expires, and settles it first.
            let delivery = Inbox::open(&db, "replacement")
                .expect("inbox")
                .lease(Handler::Verification.events(), taker, false, MAX_ATTEMPTS)
                .expect("lease")
                .expect("replacement agent's delivery");
            store::open(&db)
                .expect("db")
                .execute(
                    &format!(
                        "UPDATE deliveries SET leased_until = {EPOCH} - 1 WHERE agent = 'verifier'"
                    ),
                    [],
                )
                .expect("expire first holder's delivery");
            let mut graph = Graph::open(&db).expect("graph");
            assert!(graph.claim(step, &delivery.token).expect("take over"));
            graph
                .record_finding(step, &delivery.token, "abc", Answer::No, "replacement")
                .expect("the replacement's finding");
        });
        let agent = held.context.consumer("verifier").expect("agent");
        let reaction = Reaction::new(
            &held.context,
            &agent,
            held.holder,
            &held.delivery,
            &held.interrupt,
        );
        let report = react(&reaction, &mut held.graph).expect("reaction");
        assert!(
            matches!(&report, Report::Ignored { reason } if reason.contains("settled elsewhere")),
            "{report:?}"
        );
        let node = held.graph.node(held.step).expect("step");
        assert_eq!(node.status, NodeStatus::Completed);
        assert_eq!(node.answer, Some(Answer::No));
        assert_eq!(node.outcome.as_deref(), Some("replacement"));
        let events = held.graph.events(None, 100).expect("events");
        assert!(
            !events
                .iter()
                .any(|e| e.kind == EventKind::NodeFailed && e.node == Some(held.step)),
            "no node.failed"
        );
        let last = events.last().expect("an event");
        assert_eq!(
            (last.kind, last.node),
            (EventKind::NodeCompleted, Some(held.step))
        );
    }

    #[test]
    fn a_lease_lost_after_the_commit_publishes_nothing_of_the_attempt() {
        let mut held = held_step(false);
        let (_hook, takeover) = take_over_at(&held, Window::Publish);
        let agent = held.context.consumer("verifier").expect("agent");
        let reaction = Reaction::new(
            &held.context,
            &agent,
            held.holder,
            &held.delivery,
            &held.interrupt,
        );
        assert!(matches!(
            react(&reaction, &mut held.graph),
            Err(HandlerError::Graph(GraphError::LeaseLost(id))) if id == held.step
        ));
        let takeover = takeover.borrow();
        let replacement = takeover
            .as_ref()
            .expect("the step was taken over")
            .workspace
            .as_ref()
            .expect("the replacement's worktree");

        // The old holder did commit, on a branch of its own.
        let git = Git::new(held.context.project().root());
        let stale_branch = Workspaces::branch_name(held.step);
        assert_ne!(stale_branch, replacement.branch());
        let stale_tip = git
            .resolve_commit(&format!("refs/heads/{stale_branch}"))
            .expect("the old holder's branch");
        assert_ne!(stale_tip, git.resolve_commit("main").expect("base"));
        assert_eq!(
            git.run(&["show", &format!("{stale_tip}:old.txt")])
                .expect("the old holder's committed file"),
            "old"
        );

        // None of it reached the step: it is the replacement's branch, with
        // no tip until the replacement commits, and still running.
        let step = held.graph.node(held.step).expect("step");
        assert_eq!(step.status, NodeStatus::Running);
        assert_eq!(step.branch.as_deref(), Some(replacement.branch()));
        assert_eq!(step.head_commit, None, "no tip of the old holder's");
        assert_eq!(step.outcome, None);
        let events = held.graph.events(None, 100).expect("events");
        assert!(
            !events.iter().any(|e| e.node == Some(held.step)
                && matches!(e.kind, EventKind::NodeCompleted | EventKind::NodeFailed)),
            "the old holder settled nothing"
        );
    }

    #[test]
    fn a_retry_that_fails_before_committing_shows_no_earlier_tip() {
        let mut held = held_step(false);
        let agent = held.context.consumer("verifier").expect("agent");
        let workspaces = held.context.workspaces();
        let git = Git::new(held.context.project().root());

        // The first attempt completes with its own branch and tip, and
        // evaluation decides to carry on from it.
        let reaction = Reaction::new(
            &held.context,
            &agent,
            held.holder,
            &held.delivery,
            &held.interrupt,
        );
        let Report::Verification(first) = react(&reaction, &mut held.graph).expect("first attempt")
        else {
            panic!("expected a verification report");
        };
        assert_eq!(first.status, NodeStatus::Completed, "{}", first.summary);
        let first_tip = first.head_commit.clone().expect("first tip");
        Inbox::open(&held.db, "verifier")
            .expect("inbox")
            .ack(&held.delivery, None)
            .expect("ack");
        let completed = held.graph.last_event().expect("last").expect("completed");
        let reopened = held
            .graph
            .conclude(
                held.step,
                completed,
                &crate::graph::NewConclusion {
                    agent: "evaluator".to_owned(),
                    decision: Decision::Continue,
                    hypothesis: "s works".to_owned(),
                    finding: "half done".to_owned(),
                    consequence: "finish it".to_owned(),
                },
                &[],
            )
            .expect("conclude");
        assert_eq!(reopened, Some(true));
        let before = held.graph.node(held.step).expect("step");
        assert_eq!(before.status, NodeStatus::Planned);
        assert_eq!(before.branch.as_deref(), Some(first.branch.as_str()));
        assert_eq!(before.head_commit.as_deref(), Some(first_tip.as_str()));

        // The retry cannot commit: a hook refuses every commit.
        let hook = held.context.project().root().join(".git/hooks/pre-commit");
        std::fs::create_dir_all(hook.parent().expect("hooks")).expect("hooks directory");
        std::fs::write(&hook, "#!/bin/sh\necho 'refused by hook' >&2\nexit 1\n").expect("hook");
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("executable hook");
        let retry = Processes::open(&held.db, &held.context.project().logs_dir())
            .expect("processes")
            .create("verifier", None, false)
            .expect("retry process")
            .id;
        let delivery = Inbox::open(&held.db, "verifier")
            .expect("inbox")
            .lease(Handler::Verification.events(), retry, false, MAX_ATTEMPTS)
            .expect("lease")
            .expect("the reopened step's node.ready");
        assert_eq!(delivery.event.node, Some(held.step));

        // Before the commit, the step already shows the retry's branch, and
        // no tip: not the first attempt's.
        let seen = Rc::new(RefCell::new(None));
        let record = Rc::clone(&seen);
        let (db, step, path) = (
            held.db.clone(),
            held.step,
            workspaces.path_of(held.step, &delivery.token),
        );
        let _hook = Hooked::install(move |window| {
            if window == Window::Commit {
                // Something to commit: the agent rewrote what the first
                // attempt left unchanged.
                std::fs::write(path.join("retry.txt"), "retry\n").expect("retry's work");
                let node = Graph::open(&db).expect("graph").node(step).expect("step");
                *record.borrow_mut() = Some((node.branch, node.head_commit));
            }
        });
        let reaction = Reaction::new(&held.context, &agent, retry, &delivery, &held.interrupt);
        let Report::Verification(report) = react(&reaction, &mut held.graph).expect("retry") else {
            panic!("expected a verification report");
        };

        assert_eq!(report.status, NodeStatus::Failed, "{}", report.summary);
        assert_eq!(report.head_commit, None, "the retry committed nothing");
        assert_ne!(report.branch, first.branch, "the retry has its own branch");
        assert!(
            report.summary.contains("refused by hook"),
            "{}",
            report.summary
        );
        let (mid_branch, mid_tip) = seen.borrow_mut().take().expect("reached the commit");
        assert_eq!(mid_branch.as_deref(), Some(report.branch.as_str()));
        assert_eq!(mid_tip, None, "mid-attempt, no tip of the first attempt's");
        let after = held.graph.node(held.step).expect("step");
        assert_eq!(after.status, NodeStatus::Failed);
        assert_eq!(after.branch.as_deref(), Some(report.branch.as_str()));
        assert_eq!(after.head_commit, None, "no tip of the first attempt's");
        assert_eq!(after.outcome.as_deref(), Some(report.summary.as_str()));
        let last = held.graph.events(None, 100).expect("events");
        let last = last.last().expect("an event");
        assert_eq!(
            (last.kind, last.node),
            (EventKind::NodeFailed, Some(held.step))
        );
        // The first attempt's branch still holds its own tip.
        assert_eq!(
            git.resolve_commit(&format!("refs/heads/{}", first.branch))
                .expect("first branch"),
            first_tip
        );
    }

    #[test]
    fn a_terminally_failed_verification_leaves_its_step_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let mut graph = Graph::open(&db).expect("graph");
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
        let mut inbox = Inbox::open(&db, "verifier").expect("inbox");
        let [claimed, unclaimed] = [running, planned].map(|id| {
            let delivery = inbox
                .lease(
                    Handler::Verification.events(),
                    ProcessId(1),
                    false,
                    MAX_ATTEMPTS,
                )
                .expect("lease")
                .expect("node.ready");
            assert_eq!(delivery.event.node, Some(id));
            delivery
        });
        assert!(graph.claim(running, &claimed.token).expect("claim"));

        // Only verification leaves steps behind.
        assert_eq!(
            Handler::Planning
                .abandon(&claimed, &mut graph, "gone")
                .expect("abandon"),
            Abandonment::Nothing
        );
        assert_eq!(
            graph.node(running).expect("node").status,
            NodeStatus::Running
        );

        assert_eq!(
            Handler::Verification
                .abandon(&claimed, &mut graph, "its holder died")
                .expect("abandon"),
            Abandonment::Failed(running)
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
        for (delivery, abandonment) in [
            (&unclaimed, Abandonment::Nothing),
            (&claimed, Abandonment::Settled(running)),
        ] {
            assert_eq!(
                Handler::Verification
                    .abandon(delivery, &mut graph, "again")
                    .expect("abandon"),
                abandonment
            );
        }
        assert_eq!(
            graph.node(running).expect("node").outcome.as_deref(),
            Some("its holder died")
        );
        assert_eq!(
            graph.node(planned).expect("node").status,
            NodeStatus::Planned
        );
        assert_eq!(graph.last_event().expect("last"), before);
    }
}
