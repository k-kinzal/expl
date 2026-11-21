//! [`Declaration`] — `expl.toml`, what the operator wrote.
//!
//! The declaration names the agents available to the project, binds each
//! [`Role`](crate::role::Role) to one of them, and fixes the runner's
//! breadth and parallelism. Following iter's "no implicit defaults" rule,
//! every field the runtime needs is spelled out in the file; `expl init`
//! writes a complete [`template`](Declaration::TEMPLATE) instead of
//! leaving values to hidden fallbacks.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::AgentKind;
use crate::project::Project;
use crate::role::Role;

/// Reading or validating `expl.toml` failed.
#[derive(Debug, Error)]
pub enum DeclarationError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// File path.
        path: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML for a declaration.
    #[error("invalid {path}: {source}")]
    Parse {
        /// File path.
        path: String,
        /// Underlying error.
        #[source]
        source: toml::de::Error,
    },
    /// The declaration is well-formed but inconsistent.
    #[error("invalid declaration: {0}")]
    Invalid(String),
}

/// The whole `expl.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    /// Named agents, referenced from `[role.*]`.
    pub agent: BTreeMap<String, AgentDeclaration>,
    /// Which agent carries each role.
    pub role: RoleDeclarations,
    /// Runner breadth and parallelism.
    pub run: RunDeclaration,
}

/// One agent CLI and how to invoke it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDeclaration {
    /// Which driver translates for this CLI.
    pub kind: AgentKind,
    /// Program and leading arguments, e.g. `["claude"]`.
    pub command: Vec<String>,
    /// Extra arguments appended after the driver's own flags.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// The four role bindings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleDeclarations {
    /// Exploration (and counter-exploration).
    pub explorer: RoleDeclaration,
    /// Planning.
    pub planner: RoleDeclaration,
    /// Execution.
    pub executor: RoleDeclaration,
    /// Evaluation.
    pub evaluator: RoleDeclaration,
}

/// One role's binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleDeclaration {
    /// Name of an `[agent.<name>]` entry.
    pub agent: String,
    /// Project-specific guidance appended to the role's built-in prompt.
    #[serde(default)]
    pub instructions: Option<String>,
}

/// Runner settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunDeclaration {
    /// Maximum nodes executed (and evaluated) concurrently.
    pub parallelism: usize,
    /// Maximum nodes one exploration may propose.
    pub breadth: usize,
    /// Exploration pauses while this many nodes await execution
    /// (`proposed` + `planned`), so proposals cannot outrun execution.
    pub max_pending: usize,
}

impl Declaration {
    /// The declaration `expl init` writes.
    pub const TEMPLATE: &'static str = r#"# expl declaration. See README.md and `expl --help`..

# Agents available to this project. `kind` selects the driver:
#   claude_code — `claude --print --output-format json --json-schema ...`
#   codex       — `codex exec --output-schema ... --output-last-message ...`
#   generic     — any command: prompt on stdin, JSON response on stdout
#
# Agents run unattended inside expl-owned git worktrees, so the default
# below lets Claude Code edit files and run commands without asking.
[agent.claude]
kind = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]

# Which agent carries each role. `instructions` is appended to the role's
# built-in prompt.
[role.explorer]
agent = "claude"

[role.planner]
agent = "claude"

[role.executor]
agent = "claude"

[role.evaluator]
agent = "claude"

[run]
# Nodes executed (and evaluated) concurrently, each in its own worktree.
parallelism = 2
# Maximum nodes proposed by one exploration.
breadth = 3
# Exploration pauses while this many nodes await execution.
max_pending = 6
"#;

    /// Read and validate `path`.
    ///
    /// # Errors
    ///
    /// [`DeclarationError`] when the file is unreadable, malformed, or
    /// inconsistent.
    pub fn load(path: &Path) -> Result<Self, DeclarationError> {
        let text = std::fs::read_to_string(path).map_err(|source| DeclarationError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text).map_err(|error| match error {
            DeclarationError::Parse { source, .. } => DeclarationError::Parse {
                path: path.display().to_string(),
                source,
            },
            other @ (DeclarationError::Read { .. } | DeclarationError::Invalid(_)) => other,
        })
    }

    /// Parse and validate declaration text.
    ///
    /// # Errors
    ///
    /// [`DeclarationError::Parse`] or [`DeclarationError::Invalid`].
    pub fn parse(text: &str) -> Result<Self, DeclarationError> {
        let declaration: Self = toml::from_str(text).map_err(|source| DeclarationError::Parse {
            path: Project::DECLARATION_FILE.to_owned(),
            source,
        })?;
        declaration.validate()?;
        Ok(declaration)
    }

    fn validate(&self) -> Result<(), DeclarationError> {
        for (name, agent) in &self.agent {
            if agent.command.is_empty() || agent.command[0].trim().is_empty() {
                return Err(DeclarationError::Invalid(format!(
                    "agent `{name}` has an empty `command`"
                )));
            }
        }
        for role in Role::ALL {
            let binding = self.role_declaration(role);
            if !self.agent.contains_key(&binding.agent) {
                return Err(DeclarationError::Invalid(format!(
                    "role `{}` refers to unknown agent `{}`",
                    role.as_str(),
                    binding.agent
                )));
            }
        }
        if self.run.parallelism == 0 {
            return Err(DeclarationError::Invalid(
                "`run.parallelism` must be >= 1".into(),
            ));
        }
        if self.run.breadth == 0 {
            return Err(DeclarationError::Invalid(
                "`run.breadth` must be >= 1".into(),
            ));
        }
        Ok(())
    }

    /// The binding for `role`.
    #[must_use]
    pub fn role_declaration(&self, role: Role) -> &RoleDeclaration {
        match role {
            Role::Explorer => &self.role.explorer,
            Role::Planner => &self.role.planner,
            Role::Executor => &self.role.executor,
            Role::Evaluator => &self.role.evaluator,
        }
    }

    /// The agent declaration bound to `role` (validated to exist).
    ///
    /// # Errors
    ///
    /// [`DeclarationError::Invalid`] when the binding is dangling (only
    /// possible for a declaration built without [`Declaration::parse`]).
    pub fn agent_for(&self, role: Role) -> Result<(&str, &AgentDeclaration), DeclarationError> {
        let name = &self.role_declaration(role).agent;
        self.agent
            .get_key_value(name)
            .map(|(k, v)| (k.as_str(), v))
            .ok_or_else(|| DeclarationError::Invalid(format!("unknown agent `{name}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_is_a_valid_declaration() {
        let declaration = Declaration::parse(Declaration::TEMPLATE).expect("template parses");
        assert_eq!(declaration.run.parallelism, 2);
        assert_eq!(declaration.role.executor.agent, "claude");
    }

    #[test]
    fn dangling_role_agent_is_rejected() {
        let text = Declaration::TEMPLATE.replace(
            "[role.planner]\nagent = \"claude\"",
            "[role.planner]\nagent = \"nobody\"",
        );
        let error = Declaration::parse(&text).expect_err("dangling agent");
        assert!(
            error.to_string().contains("unknown agent `nobody`"),
            "{error}"
        );
    }
}
