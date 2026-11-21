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

use serde::Deserialize;
use serde_json::Value;

use crate::agent::drivers::{exit_failure, program};
use crate::agent::response::extract_json_object;
use crate::agent::{AgentCommand, AgentDriver, AgentError, AgentKind, AgentRequest, AgentRun};

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct ClaudeCodeDriver {
    command: Vec<String>,
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ResultRecord {
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    structured_output: Option<Value>,
    #[serde(default)]
    session_id: Option<String>,
}

impl ClaudeCodeDriver {
    /// A driver running `command --print ... args`.
    #[must_use]
    pub fn new(command: Vec<String>, args: Vec<String>) -> Self {
        Self { command, args }
    }

    /// The `result` record: the whole stdout, or failing that the last
    /// line that parses as one.
    fn result_record(stdout: &str) -> Option<ResultRecord> {
        serde_json::from_str(stdout.trim()).ok().or_else(|| {
            stdout
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str::<ResultRecord>(line.trim()).ok())
        })
    }
}

impl AgentDriver for ClaudeCodeDriver {
    fn kind(&self) -> AgentKind {
        AgentKind::ClaudeCode
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
        let stdout = String::from_utf8_lossy(&output.stdout);
        let Some(record) = Self::result_record(&stdout) else {
            return Err(if output.status.success() {
                AgentError::InvalidResponse("claude printed no result record".into())
            } else {
                exit_failure(output, "claude")
            });
        };
        if record.is_error {
            return Err(AgentError::Failed {
                code: output.status.code(),
                message: format!(
                    "claude reported {}: {}",
                    record.subtype.as_deref().unwrap_or("an error"),
                    record.result.as_deref().unwrap_or("")
                ),
            });
        }
        let response = match record.structured_output {
            Some(value @ Value::Object(_)) => value,
            Some(_) | None => record
                .result
                .as_deref()
                .and_then(extract_json_object)
                .ok_or_else(|| {
                    AgentError::InvalidResponse(
                        "claude's result carries no structured output".into(),
                    )
                })?,
        };
        Ok(AgentRun {
            session_id: record.session_id,
            response,
        })
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
