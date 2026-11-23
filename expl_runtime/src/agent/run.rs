//! [`AgentRun`].

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::agent::AgentError;

/// Result of one successful agent invocation, in expl's vocabulary.
///
/// There is deliberately no exit code: a successful run means "the agent
/// replied with a structured response"; anything else is an
/// [`AgentError`].
///
/// Drivers report the response as raw JSON (`R = serde_json::Value`);
/// [`Agent::invoke`](crate::agent::Agent::invoke) hands it on decoded into
/// the type the role expects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRun<R = serde_json::Value> {
    /// Session id reported by the CLI, when it exposes one.
    pub session_id: Option<String>,
    /// The structured final response.
    pub response: R,
}

impl AgentRun {
    /// The same run with its response decoded into `R`.
    ///
    /// # Errors
    ///
    /// [`AgentError::Mismatch`] when the response does not decode into `R`:
    /// a field is missing or invalid.
    pub fn decode<R: DeserializeOwned>(self) -> Result<AgentRun<R>, AgentError> {
        let response = serde_json::from_value(self.response).map_err(AgentError::Mismatch)?;
        Ok(AgentRun {
            session_id: self.session_id,
            response,
        })
    }
}
