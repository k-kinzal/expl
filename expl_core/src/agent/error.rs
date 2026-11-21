//! [`AgentError`].

use std::time::Duration;

use thiserror::Error;

/// An agent invocation did not yield a usable response.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The invocation could not be assembled or started.
    #[error("agent could not be launched: {0}")]
    Launch(String),
    /// I/O while talking to the child or writing its log.
    #[error("agent I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The CLI reported failure in its own dialect.
    #[error("agent failed{}: {message}", code.map(|c| format!(" (exit {c})")).unwrap_or_default())]
    Failed {
        /// Exit code, when the child exited normally.
        code: Option<i32>,
        /// What went wrong.
        message: String,
    },
    /// The process was asked to stop; the invocation was cut short.
    #[error("agent invocation interrupted")]
    Interrupted,
    /// The CLI outran its declared `timeout` and was stopped. Unlike
    /// [`Interrupted`](Self::Interrupted), this is a failure of the CLI.
    #[error("agent timed out after {}", humantime::format_duration(*after))]
    TimedOut {
        /// The `timeout` it outran.
        after: Duration,
    },
    /// The agent finished but its response is not the requested shape.
    #[error("agent response is unusable: {0}")]
    InvalidResponse(String),
}
