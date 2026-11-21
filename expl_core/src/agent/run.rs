//! [`AgentRun`].

use serde::{Deserialize, Serialize};

/// Result of one successful agent invocation, in expl's vocabulary.
///
/// There is deliberately no exit code: a successful run means "the agent
/// replied with a structured response"; anything else is an
/// [`AgentError`](crate::agent::AgentError).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRun {
    /// Session id reported by the CLI, when it exposes one.
    pub session_id: Option<String>,
    /// The structured final response.
    pub response: serde_json::Value,
}
