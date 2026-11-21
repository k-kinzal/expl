//! [`AgentKind`].

use serde::{Deserialize, Serialize};

/// The closed set of drivers, as named in `expl.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// Claude Code (`claude --print`).
    ClaudeCode,
    /// Codex (`codex exec`).
    Codex,
    /// Any command: prompt on stdin, JSON response on stdout.
    Generic,
}

impl AgentKind {
    /// Stable label for logs and records.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::Generic => "generic",
        }
    }
}
