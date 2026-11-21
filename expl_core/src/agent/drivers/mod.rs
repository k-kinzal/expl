//! Concrete [`AgentDriver`](crate::agent::AgentDriver)s, one per CLI.

mod claude_code;
mod codex;
mod generic;

pub use claude_code::ClaudeCodeDriver;
pub use codex::CodexDriver;
pub use generic::GenericDriver;

use crate::agent::AgentError;

/// Split a declared command vector into program and leading arguments.
fn program(command: &[String]) -> Result<(&str, &[String]), AgentError> {
    command
        .split_first()
        .map(|(program, rest)| (program.as_str(), rest))
        .ok_or_else(|| AgentError::Launch("empty agent command".into()))
}

/// The last few lines of `bytes`, for error messages.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let start = lines.len().saturating_sub(8);
    lines[start..].join("\n")
}

/// Map a non-success exit to [`AgentError::Failed`].
fn exit_failure(output: &std::process::Output, what: &str) -> AgentError {
    let detail = match (tail(&output.stderr), tail(&output.stdout)) {
        (stderr, _) if !stderr.is_empty() => stderr,
        (_, stdout) => stdout,
    };
    AgentError::Failed {
        code: output.status.code(),
        message: format!("{what} exited with {}: {detail}", output.status),
    }
}
