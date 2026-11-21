//! The single `result` record that Claude Code and Cursor Agent print with
//! `--print --output-format json`.
//!
//! Both CLIs can exit `0` with `is_error: true`, which is why the record is
//! read rather than the exit code trusted.

use serde::Deserialize;
use serde_json::Value;

use crate::agent::drivers::exit_failure;
use crate::agent::response::extract_json_object;
use crate::agent::{AgentError, AgentRun};

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

/// The `result` record: the whole stdout, or failing that the last line
/// that parses as one.
fn parse(stdout: &str) -> Option<ResultRecord> {
    serde_json::from_str(stdout.trim()).ok().or_else(|| {
        stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<ResultRecord>(line.trim()).ok())
    })
}

/// Read `cli`'s verdict from its `result` record: `structured_output` when
/// present, else the last JSON object in the `result` text.
pub(crate) fn interpret(output: &std::process::Output, cli: &str) -> Result<AgentRun, AgentError> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(record) = parse(&stdout) else {
        return Err(if output.status.success() {
            AgentError::InvalidResponse(format!("{cli} printed no result record"))
        } else {
            exit_failure(output, cli)
        });
    };
    if record.is_error {
        return Err(AgentError::Failed {
            code: output.status.code(),
            message: format!(
                "{cli} reported {}: {}",
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
                AgentError::InvalidResponse(format!("{cli}'s result carries no JSON object"))
            })?,
    };
    Ok(AgentRun {
        session_id: record.session_id,
        response,
    })
}
