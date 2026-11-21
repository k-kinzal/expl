//! [`AgentDriver`] — the bidirectional translator for one agent CLI.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::agent::{AgentError, AgentRun};
use crate::prompt::Prompt;

/// The closed set of drivers, as named by `driver = "..."` in `expl.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    /// Claude Code (`claude --print`).
    ClaudeCode,
    /// Codex (`codex exec`).
    Codex,
    /// Cursor Agent (`cursor-agent --print`).
    CursorAgent,
    /// Any command: prompt on stdin, JSON response on stdout.
    Generic,
}

impl DriverKind {
    /// Stable label for logs and records.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::CursorAgent => "cursor_agent",
            Self::Generic => "generic",
        }
    }
}

/// What expl asks of one invocation.
#[derive(Debug, Clone, Copy)]
pub struct AgentRequest<'a> {
    /// The rendered instructions.
    pub prompt: &'a Prompt,
    /// JSON Schema the final response must satisfy.
    pub response_schema: &'a serde_json::Value,
    /// A private directory for command-line artefacts (schema files,
    /// last-message files). Lives until the run is interpreted.
    pub scratch: &'a Path,
    /// What the reaction is about, as `EXPL_*` environment for wrapper
    /// commands. Drivers for AI CLIs drop it: the agent needs only its
    /// task, never the fact that it runs under expl.
    pub facts: &'a [(String, String)],
}

/// One assembled child-process invocation — a thing, not an act: the
/// [`Agent`](crate::agent::Agent) performs the invoking.
#[derive(Debug)]
pub struct AgentCommand {
    /// Program, arguments, environment. The working directory is imposed
    /// by the agent at spawn time.
    pub process: Command,
    /// Text to feed on stdin, if any.
    pub stdin: Option<String>,
}

/// The bidirectional translator for one agent CLI.
pub trait AgentDriver: Send + Sync {
    /// The closed discriminant of this driver.
    fn kind(&self) -> DriverKind;

    /// Outbound: assemble one invocation for `request`.
    ///
    /// # Errors
    ///
    /// [`AgentError::Launch`] when the invocation cannot be assembled.
    fn command(&self, request: &AgentRequest<'_>) -> Result<AgentCommand, AgentError>;

    /// Inbound: read the CLI's verdict out of its output and exit status.
    ///
    /// # Errors
    ///
    /// [`AgentError`] when the run did not end in a usable response.
    fn interpret(
        &self,
        output: &std::process::Output,
        request: &AgentRequest<'_>,
    ) -> Result<AgentRun, AgentError>;
}
