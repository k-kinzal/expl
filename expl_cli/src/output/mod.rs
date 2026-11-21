//! Rendering: tables and NDJSON on stdout, diagnostics on stderr.
//!
//! As in iter, stdout carries data that scripts may capture; human status
//! and errors go to stderr.

mod table;

pub(crate) use table::Table;

use std::io::Write;

use crate::error::CliError;

/// Write `text` to stdout.
pub(crate) fn print(text: &str) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| CliError::io("write stdout", e))
}

/// Write one line to stdout.
pub(crate) fn println(line: &str) -> Result<(), CliError> {
    print(&format!("{line}\n"))
}

/// Write one status line to stderr.
pub(crate) fn status(line: &str) {
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "expl: {line}").ok();
}

/// Print `error` and its causal chain to stderr.
pub(crate) fn report_error(error: &CliError) {
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "error: {error}").ok();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !error.to_string().contains(&text) {
            writeln!(stderr, "  caused by: {text}").ok();
        }
        source = cause.source();
    }
}

/// Serialise one value as a JSON line.
pub(crate) fn json_line(value: &impl serde::Serialize) -> Result<(), CliError> {
    let text = serde_json::to_string(value)
        .map_err(|e| CliError::io("encode json", std::io::Error::other(e)))?;
    println(&text)
}

/// Serialise one value as pretty JSON.
pub(crate) fn json_pretty(value: &impl serde::Serialize) -> Result<(), CliError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| CliError::io("encode json", std::io::Error::other(e)))?;
    println(&text)
}

/// Shorten free text to one line of at most `width` characters.
pub(crate) fn clip(text: &str, width: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= width {
        flat
    } else {
        let cut: String = flat.chars().take(width.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// The first 7 characters of a commit id.
pub(crate) fn short_commit(commit: Option<&str>) -> String {
    commit.map_or_else(|| "-".to_owned(), |c| c.chars().take(7).collect())
}
