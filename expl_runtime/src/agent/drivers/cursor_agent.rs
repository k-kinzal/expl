//! [`CursorAgentDriver`] — Cursor Agent CLI integration.
//!
//! Assembles:
//!
//! ```text
//! cursor-agent --print --output-format json [args...]
//! ```
//!
//! …with the prompt on stdin. Cursor Agent has no schema flag, so the
//! response schema reaches it only through the prompt; its single `result`
//! record has the same shape as Claude Code's, and the structured response
//! is the last JSON object in the `result` text.

use std::process::Command;

use crate::agent::drivers::{program, result_record};
use crate::agent::{AgentCommand, AgentDriver, AgentError, AgentRequest, AgentRun, DriverKind};

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct CursorAgentDriver {
    command: Vec<String>,
    args: Vec<String>,
}

impl CursorAgentDriver {
    /// A driver running `command --print --output-format json args`.
    #[must_use]
    pub fn new(command: Vec<String>, args: Vec<String>) -> Self {
        Self { command, args }
    }
}

impl AgentDriver for CursorAgentDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::CursorAgent
    }

    fn command(&self, request: &AgentRequest<'_>) -> Result<AgentCommand, AgentError> {
        let (program, leading) = program(&self.command)?;
        let mut process = Command::new(program);
        process
            .args(leading)
            .arg("--print")
            .arg("--output-format")
            .arg("json")
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
        result_record::interpret(output, "cursor-agent")
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
        let driver = CursorAgentDriver::new(vec!["cursor-agent".into()], vec![]);
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
    fn reads_the_object_in_the_result_text() {
        let run = interpret(&output(
            0,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.\n```json\n{\"succeeded\": true}\n```","session_id":"c"}"#,
        ))
        .expect("run");
        assert_eq!(run.response, serde_json::json!({"succeeded": true}));
        assert_eq!(run.session_id.as_deref(), Some("c"));
    }

    #[test]
    fn is_error_fails() {
        let error = interpret(&output(
            0,
            r#"{"type":"result","subtype":"error","is_error":true,"result":"quota"}"#,
        ))
        .expect_err("is_error");
        assert!(
            error.to_string().contains("cursor-agent reported"),
            "{error}"
        );
    }

    #[test]
    fn prompt_goes_to_stdin_and_args_follow_the_flags() {
        let driver = CursorAgentDriver::new(
            vec!["cursor-agent".into()],
            vec!["--model".into(), "m".into()],
        );
        let prompt = crate::Prompt::from("task");
        let schema = serde_json::json!({});
        let request = AgentRequest {
            prompt: &prompt,
            response_schema: &schema,
            scratch: std::path::Path::new("/nonexistent"),
            facts: &[],
        };
        let command = driver.command(&request).expect("command");
        let args: Vec<_> = command
            .process
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--print", "--output-format", "json", "--model", "m"]);
        assert_eq!(command.stdin.as_deref(), Some("task"));
    }
}
