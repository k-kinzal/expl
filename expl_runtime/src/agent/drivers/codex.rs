//! [`CodexDriver`] — Codex CLI integration.
//!
//! Assembles:
//!
//! ```text
//! codex exec --output-schema <scratch>/response-schema.json \
//!            --output-last-message <scratch>/last-message.txt [args...] -
//! ```
//!
//! …with the prompt on stdin (`-`). Codex validates the final message
//! against the schema; the driver reads it back from the last-message file.

use std::process::Command;

use crate::agent::drivers::{exit_failure, program};
use crate::agent::response::extract_json_object;
use crate::agent::{AgentCommand, AgentDriver, AgentError, AgentRequest, AgentRun, DriverKind};

const SCHEMA_FILE: &str = "response-schema.json";
const LAST_MESSAGE_FILE: &str = "last-message.txt";

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct CodexDriver {
    command: Vec<String>,
    args: Vec<String>,
}

impl CodexDriver {
    /// A driver running `command exec ... args -`.
    #[must_use]
    pub fn new(command: Vec<String>, args: Vec<String>) -> Self {
        Self { command, args }
    }
}

impl AgentDriver for CodexDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Codex
    }

    fn command(&self, request: &AgentRequest<'_>) -> Result<AgentCommand, AgentError> {
        let (program, leading) = program(&self.command)?;
        let schema_path = request.scratch.join(SCHEMA_FILE);
        std::fs::write(&schema_path, request.response_schema.to_string())?;
        let mut process = Command::new(program);
        process
            .args(leading)
            .arg("exec")
            .arg("--output-schema")
            .arg(&schema_path)
            .arg("--output-last-message")
            .arg(request.scratch.join(LAST_MESSAGE_FILE))
            .args(&self.args)
            .arg("-");
        Ok(AgentCommand {
            process,
            stdin: Some(request.prompt.as_str().to_owned()),
        })
    }

    fn interpret(
        &self,
        output: &std::process::Output,
        request: &AgentRequest<'_>,
    ) -> Result<AgentRun, AgentError> {
        if !output.status.success() {
            return Err(exit_failure(output, "codex"));
        }
        let message =
            std::fs::read_to_string(request.scratch.join(LAST_MESSAGE_FILE)).map_err(|error| {
                AgentError::InvalidResponse(format!("codex wrote no last message: {error}"))
            })?;
        let response = extract_json_object(&message).ok_or_else(|| {
            AgentError::InvalidResponse("codex's last message is not a JSON object".into())
        })?;
        Ok(AgentRun {
            session_id: None,
            response,
        })
    }
}
