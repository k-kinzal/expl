//! [`Prompt`] — the text handed to an agent for one invocation.

use std::fmt;

/// The rendered instructions for one agent invocation.
///
/// A newtype rather than a bare `String` so signatures say *what* the text
/// is: roles render prompts, drivers deliver them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt(String);

impl Prompt {
    /// The prompt text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Prompt {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Prompt {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for Prompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
