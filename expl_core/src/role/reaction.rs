//! [`Reaction`] — one agent responding to one delivered event.

use std::path::Path;

use serde::de::DeserializeOwned;

use crate::agent::{Agent, AgentError, Invocation};
use crate::event::{Delivery, Event};
use crate::graph::{AgentRunStatus, Graph, NodeId};
use crate::process::{Interrupt, ProcessId};
use crate::prompt::Prompt;
use crate::role::{Context, Report, RoleError};

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

    /// Invoke the agent once in `directory` and decode its response.
    ///
    /// The invocation is journaled (`agent_runs`) with its raw log under
    /// `.expl/logs/`, whether it succeeds or not.
    pub(crate) fn invoke<T: DeserializeOwned>(
        &self,
        graph: &mut Graph,
        node: Option<NodeId>,
        directory: &Path,
        prompt: &Prompt,
        schema: &serde_json::Value,
    ) -> Result<T, RoleError> {
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
        let outcome = self
            .agent
            .invoke(&Invocation {
                directory,
                prompt,
                response_schema: schema,
                facts: &facts,
                log: &log,
                interrupt: self.interrupt,
            })
            .map_err(RoleError::from)
            .and_then(|run| {
                serde_json::from_value::<T>(run.response)
                    .map(|decoded| (decoded, run.session_id))
                    .map_err(|source| RoleError::Response {
                        role: role.as_str(),
                        source,
                    })
            });
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
