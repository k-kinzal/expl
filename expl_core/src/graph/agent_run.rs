//! [`AgentRunRecord`] — the journal of agent invocations.

use serde::{Deserialize, Serialize};

use crate::graph::NodeId;
use crate::graph::node::text_enum;

/// Identity of an agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentRunId(pub i64);

/// Outcome of an agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunStatus {
    /// In progress.
    Running,
    /// The agent produced a valid response.
    Succeeded,
    /// The agent failed or its response was unusable.
    Failed,
    /// The process was stopped mid-run.
    Interrupted,
}

text_enum!(AgentRunStatus {
    Running => "running",
    Succeeded => "succeeded",
    Failed => "failed",
    Interrupted => "interrupted",
});

/// One agent invocation for one role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRunRecord {
    /// Identity.
    pub id: AgentRunId,
    /// Declared agent name.
    pub agent: String,
    /// The agent's role (`exploration`, `planning`, `execution`, `evaluation`).
    pub role: String,
    /// The process that ran it.
    pub process: Option<i64>,
    /// The event it reacted to.
    pub event: Option<i64>,
    /// The node the run was about, when any.
    pub node: Option<NodeId>,
    /// Outcome.
    pub status: AgentRunStatus,
    /// Raw stdout/stderr log.
    pub log_path: Option<String>,
    /// Session id reported by the agent CLI.
    pub session_id: Option<String>,
    /// Failure message.
    pub error: Option<String>,
    /// Start time.
    pub started_at: String,
    /// End time.
    pub finished_at: Option<String>,
}
