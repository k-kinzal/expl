//! [`ClaudeCodeDriver`] — Claude Code CLI integration.
//!
//! Assembles:
//!
//! ```text
//! claude --print --output-format json --json-schema <schema> [args...]
//! ```
//!
//! …with the prompt on stdin. The single `result` record on stdout is the
//! CLI's verdict: it can exit `0` with `is_error: true`, which is why the
//! inbound direction reads the record rather than trusting the exit code.
//! The structured response comes from `structured_output`, falling back to
//! a JSON object embedded in the `result` text.

use std::process::Command;

use crate::agent::drivers::{program, result_record};
use crate::agent::{AgentCommand, AgentDriver, AgentError, AgentRequest, AgentRun, DriverKind};

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct ClaudeCodeDriver {
    command: Vec<String>,
    args: Vec<String>,
}

impl ClaudeCodeDriver {
    /// A driver running `command --print ... args`.
    #[must_use]
    pub fn new(command: Vec<String>, args: Vec<String>) -> Self {
        Self { command, args }
    }
}

impl AgentDriver for ClaudeCodeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::ClaudeCode
    }

    fn command(&self, request: &AgentRequest<'_>) -> Result<AgentCommand, AgentError> {
        let (program, leading) = program(&self.command)?;
        let mut process = Command::new(program);
        process
            .args(leading)
            .arg("--print")
            .arg("--output-format")
            .arg("json")
            .arg("--json-schema")
            .arg(request.response_schema.to_string())
            .args(&self.args);
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
        result_record::interpret(output, "claude")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn output(code: i32, stdout: &str) -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn interpret(out: &std::process::Output) -> Result<AgentRun, AgentError> {
        let driver = ClaudeCodeDriver::new(vec!["claude".into()], vec![]);
        let prompt = crate::Prompt::from("p");
        let schema = serde_json::json!({});
        let request = AgentRequest {
            prompt: &prompt,
            response_schema: &schema,
            scratch: std::path::Path::new("/nonexistent"),
            facts: &[],
        };
        driver.interpret(out, &request)
    }

    #[test]
    fn structured_output_wins() {
        let run = interpret(&output(
            0,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"a\":0}","structured_output":{"a":1},"session_id":"s"}"#,
        ))
        .expect("run");
        assert_eq!(run.response, serde_json::json!({"a":1}));
        assert_eq!(run.session_id.as_deref(), Some("s"));
    }

    #[test]
    fn falls_back_to_result_text() {
        let run = interpret(&output(
            0,
            r#"{"type":"result","is_error":false,"result":"done: {\"a\":2}"}"#,
        ))
        .expect("run");
        assert_eq!(run.response, serde_json::json!({"a":2}));
    }

    #[test]
    fn is_error_fails_even_on_exit_zero() {
        let error = interpret(&output(
            0,
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"x"}"#,
        ))
        .expect_err("is_error");
        assert!(error.to_string().contains("error_max_turns"), "{error}");
    }

    #[test]
    fn no_record_on_failure_is_exit_failure() {
        let error = interpret(&output(1, "boom")).expect_err("failure");
        assert!(
            matches!(error, AgentError::Failed { code: Some(1), .. }),
            "{error}"
        );
    }
}
