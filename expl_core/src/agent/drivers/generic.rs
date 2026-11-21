//! [`GenericDriver`] — any command that reads a prompt on stdin and prints
//! a JSON object on stdout.
//!
//! The response schema is written to a scratch file whose path is exported
//! as `EXPL_RESPONSE_SCHEMA`, so wrappers around other CLIs can forward it,
//! alongside the reaction's facts (`EXPL_AGENT`, `EXPL_ROLE`, `EXPL_EVENT`,
//! `EXPL_NODE`). Only this driver exports them.
//! Also the driver for scripted agents in tests.

use std::process::Command;

use crate::agent::drivers::{exit_failure, program};
use crate::agent::response::extract_json_object;
use crate::agent::{AgentCommand, AgentDriver, AgentError, AgentRequest, AgentRun, DriverKind};

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct GenericDriver {
    command: Vec<String>,
    args: Vec<String>,
}

impl GenericDriver {
    /// A driver running `command` followed by `args`.
    #[must_use]
    pub fn new(command: Vec<String>, args: Vec<String>) -> Self {
        Self { command, args }
    }
}

impl AgentDriver for GenericDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Generic
    }

    fn command(&self, request: &AgentRequest<'_>) -> Result<AgentCommand, AgentError> {
        let (program, leading) = program(&self.command)?;
        let schema_path = request.scratch.join("response-schema.json");
        std::fs::write(&schema_path, request.response_schema.to_string())?;
        let mut process = Command::new(program);
        process
            .args(leading)
            .args(&self.args)
            .envs(request.facts.iter().map(|(k, v)| (k, v)))
            .env("EXPL_RESPONSE_SCHEMA", &schema_path);
        Ok(AgentCommand {
            process,
            stdin: Some(request.prompt.as_str().to_owned()),
        })
    }

    fn interpret(
        &self,
        output: &std::process::Output,
        _request: &AgentRequest<'_>,
    ) -> Result<AgentRun, AgentError> {
        if !output.status.success() {
            return Err(exit_failure(output, "agent command"));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let response = extract_json_object(&stdout)
            .ok_or_else(|| AgentError::InvalidResponse("no JSON object found on stdout".into()))?;
        Ok(AgentRun {
            session_id: None,
            response,
        })
    }
}
