//! [`Reaction`] — one agent responding to one delivered event.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use serde::de::DeserializeOwned;

use crate::agent::{Agent, AgentError, Invocation};
use crate::event::{Delivery, Event, LeaseToken};
use crate::git::Git;
use crate::graph::{AgentRunStatus, Graph, NodeId};
use crate::process::{Interrupt, ProcessId};
use crate::prompt::Prompt;
use crate::role::{Context, Report, RoleError};
use crate::workspace::{ScratchWorkspace, Workspace};

/// Everything a role needs to react to one event.
#[derive(Debug, Clone, Copy)]
pub struct Reaction<'a> {
    context: &'a Context,
    agent: &'a Agent,
    process: ProcessId,
    delivery: &'a Delivery,
    interrupt: &'a Interrupt,
}

impl<'a> Reaction<'a> {
    /// `agent`, hosted by `process`, reacting to `delivery`.
    #[must_use]
    pub fn new(
        context: &'a Context,
        agent: &'a Agent,
        process: ProcessId,
        delivery: &'a Delivery,
        interrupt: &'a Interrupt,
    ) -> Self {
        Self {
            context,
            agent,
            process,
            delivery,
            interrupt,
        }
    }

    /// The project.
    #[must_use]
    pub fn context(&self) -> &'a Context {
        self.context
    }

    /// The reacting agent.
    #[must_use]
    pub fn agent(&self) -> &'a Agent {
        self.agent
    }

    /// The reaction's lease token.
    pub(crate) fn token(&self) -> &'a LeaseToken {
        &self.delivery.token
    }

    /// The event reacted to.
    #[must_use]
    pub fn event(&self) -> &'a Event {
        &self.delivery.event
    }

    /// How many times this event has been delivered to this agent.
    #[must_use]
    pub fn attempts(&self) -> i64 {
        self.delivery.attempts
    }

    /// Whether the process lost its lease on the event, so another process
    /// may already be reacting to it. The reaction is interrupted then, and
    /// must leave the graph to the new holder.
    #[must_use]
    pub fn lease_lost(&self) -> bool {
        self.interrupt.lease_lost()
    }

    /// The node the event is about.
    ///
    /// # Errors
    ///
    /// [`RoleError::Malformed`] when the event names no node.
    pub(crate) fn subject(&self) -> Result<NodeId, RoleError> {
        self.event().node.ok_or_else(|| RoleError::Malformed {
            role: self.agent.role().as_str(),
            event: self.event().to_string(),
            reason: "it names no node".into(),
        })
    }

    /// A report for an event that needs no reaction.
    pub(crate) fn ignore(reason: impl Into<String>) -> Report {
        Report::Ignored {
            reason: reason.into(),
        }
    }

    /// Invoke the agent once in the step's own `workspace` and decode its
    /// response.
    ///
    /// The invocation is journaled (`agent_runs`) with its raw log under
    /// `.expl/logs/`, whether it succeeds or not.
    pub(crate) fn invoke<T: DeserializeOwned>(
        &self,
        graph: &mut Graph,
        node: Option<NodeId>,
        workspace: &Workspace,
        prompt: &Prompt,
        schema: &serde_json::Value,
    ) -> Result<T, RoleError> {
        self.run(graph, node, Place::Step(workspace), prompt, schema)
    }

    /// Invoke the agent once in the read-only `scratch` worktree, as
    /// [`invoke`](Self::invoke) does, then check that the worktree is
    /// unchanged.
    ///
    /// # Errors
    ///
    /// [`RoleError::ScratchModified`], naming what changed, when the agent
    /// moved HEAD, changed files, or left untracked files — whether or not
    /// it replied. The violation is appended to the run's log and the run
    /// is journaled as failed with it; the response is discarded.
    pub(crate) fn invoke_read_only<T: DeserializeOwned>(
        &self,
        graph: &mut Graph,
        node: Option<NodeId>,
        scratch: &ScratchWorkspace,
        prompt: &Prompt,
        schema: &serde_json::Value,
    ) -> Result<T, RoleError> {
        self.run(graph, node, Place::Scratch(scratch), prompt, schema)
    }

    fn run<T: DeserializeOwned>(
        &self,
        graph: &mut Graph,
        node: Option<NodeId>,
        place: Place<'_>,
        prompt: &Prompt,
        schema: &serde_json::Value,
    ) -> Result<T, RoleError> {
        let (directory, scratch) = match place {
            Place::Step(workspace) => (workspace.path(), None),
            Place::Scratch(scratch) => (scratch.path(), Some(scratch)),
        };
        let role = self.agent.role();
        let run_id = graph.begin_agent_run(
            self.agent.name(),
            role.as_str(),
            Some(self.process.0),
            Some(self.event().id),
            node,
        )?;
        let logs = self.context.project().logs_dir();
        std::fs::create_dir_all(&logs)?;
        let log = logs.join(format!("{:05}-{}.log", run_id.0, self.agent.name()));
        graph.set_agent_run_log(run_id, &log)?;

        let mut facts = vec![
            ("EXPL_AGENT".to_owned(), self.agent.name().to_owned()),
            ("EXPL_ROLE".to_owned(), role.as_str().to_owned()),
            (
                "EXPL_EVENT".to_owned(),
                self.event().kind.as_str().to_owned(),
            ),
        ];
        if let Some(node) = node {
            facts.push(("EXPL_NODE".to_owned(), node.0.to_string()));
        }
        // Every directory an agent works in is a worktree expl made for it,
        // so a failed attempt's leftovers can be reset before the next CLI.
        // Not while it resolves a merge: resetting would abort the merge
        // the next CLI is to finish. Nor once the worktree left the branch
        // it holds (or, for a scratch worktree, its detached HEAD): the
        // reset would move whatever branch HEAD points at now.
        let git = Git::in_worktree(directory);
        let merging = git
            .run(&["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .is_ok();
        let started_at = (!merging)
            .then(|| git.resolve_commit("HEAD").ok())
            .flatten();
        let restore = || -> std::io::Result<()> {
            let restored = match (&started_at, place) {
                (None, _) => Ok(()),
                (Some(head), Place::Step(workspace)) => workspace.restore(head),
                (Some(_), Place::Scratch(scratch)) => scratch.restore(),
            };
            restored.map_err(std::io::Error::other)
        };
        let outcome = self
            .agent
            .invoke::<T>(&Invocation {
                directory,
                prompt,
                response_schema: schema,
                facts: &facts,
                log: &log,
                interrupt: self.interrupt,
                restore: Some(&restore),
            })
            .map(|run| (run.response, run.session_id))
            .map_err(RoleError::from);
        let outcome = match scratch {
            Some(scratch) if !matches!(outcome, Err(RoleError::Agent(AgentError::Interrupted))) => {
                match scratch.changes() {
                    Ok(changes) if changes.is_empty() => outcome,
                    Ok(changes) => {
                        let violation = RoleError::ScratchModified {
                            role: role.as_str(),
                            changes,
                        };
                        append_to_log(&log, &violation, outcome.as_ref().err());
                        Err(violation)
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Some(_) | None => outcome,
        };
        match outcome {
            Ok((decoded, session_id)) => {
                graph.finish_agent_run(
                    run_id,
                    AgentRunStatus::Succeeded,
                    session_id.as_deref(),
                    None,
                )?;
                Ok(decoded)
            }
            Err(error) => {
                let status = if matches!(error, RoleError::Agent(AgentError::Interrupted)) {
                    AgentRunStatus::Interrupted
                } else {
                    AgentRunStatus::Failed
                };
                graph.finish_agent_run(run_id, status, None, Some(&error.to_string()))?;
                Err(error)
            }
        }
    }
}

/// Where an agent is invoked.
#[derive(Debug, Clone, Copy)]
enum Place<'a> {
    /// A change step's own worktree, on the step's branch.
    Step(&'a Workspace),
    /// A read-only scratch worktree, detached.
    Scratch(&'a ScratchWorkspace),
}

/// Append a read-only `violation` (and the agent's own `failure`, which it
/// supersedes) to the run's raw log. Best effort: the run's journaled
/// error records the violation either way.
fn append_to_log(log: &Path, violation: &RoleError, failure: Option<&RoleError>) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log) {
        writeln!(file, "\n[expl] {violation}").ok();
        if let Some(failure) = failure {
            writeln!(file, "[expl] the run had also failed: {failure}").ok();
        }
    }
}
