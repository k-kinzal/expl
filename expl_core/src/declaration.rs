//! [`Declaration`] — `expl.toml`, what the operator wrote.
//!
//! The declaration names the project's [agents](crate::agent::Agent): for
//! each, its role, the AI CLI that carries it out (and alternative CLIs its
//! invocations are routed across), and how many processes `expl up` keeps
//! running. Following iter's "no implicit defaults" rule,
//! every field the runtime needs is spelled out; `expl init` writes a
//! complete [`TEMPLATE`](Declaration::TEMPLATE).

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::DriverKind;
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
    /// No agent has this name.
    #[error("no agent named `{0}` in expl.toml")]
    UnknownAgent(String),
    /// An agent's `timeout` (or one of its alternatives') is zero or not a
    /// duration.
    #[error("agent `{agent}`: invalid `timeout`: {source}")]
    Timeout {
        /// The agent declaring it.
        agent: String,
        /// Why the value is unusable.
        #[source]
        source: TimeoutError,
    },
}

/// A declared [`Timeout`] is not a usable duration.
#[derive(Debug, Error)]
pub enum TimeoutError {
    /// The value is zero or negative.
    #[error("`{0}` is not a positive duration")]
    NotPositive(Timeout),
    /// The text is not a duration.
    #[error("`{value}` is not a duration: {source}")]
    Unparsable {
        /// The declared text.
        value: String,
        /// Underlying error.
        #[source]
        source: humantime::DurationError,
    },
}

/// How long one CLI attempt may run before expl stops it: whole seconds
/// (`timeout = 1800`) or a humantime string (`timeout = "30m"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Timeout {
    /// Whole seconds.
    Seconds(i64),
    /// A humantime duration such as `"90s"` or `"1h 30m"`.
    Text(String),
}

impl Timeout {
    /// The declared duration.
    ///
    /// # Errors
    ///
    /// [`TimeoutError`] when the value is zero, negative, or not a duration.
    pub fn duration(&self) -> Result<Duration, TimeoutError> {
        let duration = match self {
            Self::Seconds(seconds) => u64::try_from(*seconds)
                .map(Duration::from_secs)
                .map_err(|_| TimeoutError::NotPositive(self.clone()))?,
            Self::Text(text) => humantime::parse_duration(text.trim()).map_err(|source| {
                TimeoutError::Unparsable {
                    value: text.clone(),
                    source,
                }
            })?,
        };
        if duration.is_zero() {
            return Err(TimeoutError::NotPositive(self.clone()));
        }
        Ok(duration)
    }
}

impl fmt::Display for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Seconds(seconds) => write!(f, "{seconds}"),
            Self::Text(text) => f.write_str(text),
        }
    }
}

/// The whole `expl.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    /// The default branch expl follows: whenever it moves, the origin
    /// advances to it and exploration starts again from there.
    #[serde(default)]
    pub trunk: Option<String>,
    /// Named agents.
    pub agent: BTreeMap<String, AgentDeclaration>,
}

/// One `[agent.<name>]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDeclaration {
    /// What the agent does, and so which events it reacts to.
    pub role: Role,
    /// Which driver translates for its CLI.
    pub driver: DriverKind,
    /// Program and leading arguments, e.g. `["claude"]`.
    pub command: Vec<String>,
    /// Extra arguments appended after the driver's own flags.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the CLI.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Project-specific guidance appended to the role's built-in prompt.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Processes `expl up` keeps running.
    pub replicas: usize,
    /// Exploration only: maximum steps proposed per reaction.
    #[serde(default)]
    pub breadth: Option<usize>,
    /// Exploration only: pause while this many steps await execution.
    #[serde(default)]
    pub max_pending: Option<usize>,
    /// Exploration only: seconds an idle process waits after its last
    /// exploration before it revisits a random starting point. Without it,
    /// an exploration reacts to events only and never revisits.
    #[serde(default)]
    pub revisit_cooldown: Option<u64>,
    /// How long one attempt of a CLI may run before its process group is
    /// stopped and the attempt fails. Applies to the primary CLI and to
    /// every alternative that does not declare its own. Without it, an
    /// attempt runs until the CLI exits.
    #[serde(default)]
    pub timeout: Option<Timeout>,
    /// Further CLIs that can carry out the agent (`[[agent.<name>.alternative]]`).
    /// Invocations are routed across the primary CLI and these in turn, and
    /// fall over to the next one when a CLI fails.
    #[serde(default)]
    pub alternative: Vec<CliDeclaration>,
}

/// One alternative CLI of an agent: the same fields that declare the
/// agent's primary CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliDeclaration {
    /// Which driver translates for this CLI.
    pub driver: DriverKind,
    /// Program and leading arguments, e.g. `["codex"]`.
    pub command: Vec<String>,
    /// Extra arguments appended after the driver's own flags.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for this CLI, over the agent's `env`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// How long one attempt of this CLI may run, over the agent's `timeout`.
    #[serde(default)]
    pub timeout: Option<Timeout>,
}

impl AgentDeclaration {
    /// Every CLI of the agent, primary first, each with the `timeout` that
    /// bounds it (its own, else the agent's).
    #[must_use]
    pub fn clis(&self) -> Vec<CliDeclaration> {
        let primary = CliDeclaration {
            driver: self.driver,
            command: self.command.clone(),
            args: self.args.clone(),
            env: BTreeMap::new(),
            timeout: self.timeout.clone(),
        };
        let alternatives = self.alternative.iter().map(|cli| CliDeclaration {
            timeout: cli.timeout.clone().or_else(|| self.timeout.clone()),
            ..cli.clone()
        });
        std::iter::once(primary).chain(alternatives).collect()
    }
}

impl Declaration {
    /// The declaration `expl init` writes.
    pub const TEMPLATE: &'static str = r#"# expl declaration. See README.md and `expl --help`.
#
# Each [agent.<name>] is started as processes (`expl run <name>`, or
# `expl up` for `replicas` of every agent). An agent reacts to the graph
# events of its role:
#   exploration — goal.added, node.evaluated, origin.advanced → proposes steps;
#                 when idle, every `revisit_cooldown` seconds it explores again
#                 from a random starting point (node.revisited)
#   planning    — node.proposed/evaluated/failed, goal.retired, origin.advanced → paths and priorities
#   execution   — node.ready                       → commits on the step's branch
#   evaluation  — node.completed, origin.advanced → verdicts against goals
#
# `driver` selects how the CLI is spoken to:
#   claude_code  — `claude --print --output-format json --json-schema ...`
#   codex        — `codex exec --output-schema ... --output-last-message ...`
#   cursor_agent — `cursor-agent --print --output-format json ...`
#   generic      — any command: prompt on stdin, JSON response on stdout
#
# An agent may name further CLIs with [[agent.<name>.alternative]] (driver,
# command, args, env). Its invocations are then routed across all of them in
# turn, falling over to the next CLI when one fails.
#
# An optional `timeout` (seconds, or a duration such as "30m") bounds each
# CLI attempt: past it, the CLI is stopped and the attempt fails like any
# other. An alternative's own `timeout` overrides the agent's.
#
# Agents run unattended inside expl-owned git worktrees, so the defaults
# below let Claude Code edit files and run commands without asking.

# The default branch. Merge the candidates you choose into it with git;
# running processes notice it moved, the origin follows, and exploration
# starts again from there.
trunk = "main"

[agent.explorer]
role = "exploration"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 1
breadth = 3
max_pending = 6
revisit_cooldown = 600          # idle: explore again from a random node every 10 minutes

[agent.planner]
role = "planning"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 1

[agent.executor]
role = "execution"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 2

[agent.evaluator]
role = "evaluation"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 1
"#;

    /// The declaration `expl init` writes, following `trunk` (or with the
    /// `trunk` line commented out when there is none).
    #[must_use]
    pub fn template(trunk: Option<&str>) -> String {
        let line = "trunk = \"main\"";
        let replacement = trunk.map_or_else(
            || "# trunk = \"main\"".to_owned(),
            |t| format!("trunk = {}", toml::Value::String(t.to_owned())),
        );
        Self::TEMPLATE.replacen(line, &replacement, 1)
    }

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
        let declaration: Self =
            toml::from_str(&text).map_err(|source| DeclarationError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        declaration.validate()?;
        Ok(declaration)
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
        let invalid = |name: &str, what: &str| {
            Err(DeclarationError::Invalid(format!("agent `{name}`: {what}")))
        };
        for (name, agent) in &self.agent {
            if agent
                .clis()
                .iter()
                .any(|cli| cli.command.first().is_none_or(|p| p.trim().is_empty()))
            {
                return invalid(name, "`command` is empty");
            }
            for timeout in agent.clis().iter().filter_map(|cli| cli.timeout.as_ref()) {
                timeout
                    .duration()
                    .map_err(|source| DeclarationError::Timeout {
                        agent: name.clone(),
                        source,
                    })?;
            }
            match agent.role {
                Role::Exploration => {
                    if agent.breadth.is_none_or(|b| b == 0) {
                        return invalid(name, "exploration needs `breadth` >= 1");
                    }
                    if agent.max_pending.is_none_or(|m| m == 0) {
                        return invalid(name, "exploration needs `max_pending` >= 1");
                    }
                }
                Role::Planning | Role::Execution | Role::Evaluation => {
                    if agent.breadth.is_some()
                        || agent.max_pending.is_some()
                        || agent.revisit_cooldown.is_some()
                    {
                        return invalid(
                            name,
                            "`breadth`, `max_pending` and `revisit_cooldown` apply to exploration \
                             agents only",
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// The agent named `name`.
    ///
    /// # Errors
    ///
    /// [`DeclarationError::UnknownAgent`].
    pub fn agent(&self, name: &str) -> Result<&AgentDeclaration, DeclarationError> {
        self.agent
            .get(name)
            .ok_or_else(|| DeclarationError::UnknownAgent(name.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_is_a_valid_declaration() {
        let declaration = Declaration::parse(Declaration::TEMPLATE).expect("template parses");
        assert_eq!(declaration.agent.len(), 4);
        assert_eq!(declaration.agent["executor"].role, Role::Execution);
        assert_eq!(declaration.agent["executor"].replicas, 2);
        assert_eq!(declaration.trunk.as_deref(), Some("main"));
        let none = Declaration::parse(&Declaration::template(None)).expect("parses");
        assert_eq!(none.trunk, None);
        let dev = Declaration::parse(&Declaration::template(Some("dev"))).expect("parses");
        assert_eq!(dev.trunk.as_deref(), Some("dev"));
    }

    #[test]
    fn exploration_settings_are_role_specific() {
        let text = Declaration::TEMPLATE
            .replace("role = \"planning\"", "role = \"planning\"\nbreadth = 2");
        let error = Declaration::parse(&text).expect_err("breadth on planner");
        assert!(
            error.to_string().contains("exploration agents only"),
            "{error}"
        );
        let text = Declaration::TEMPLATE.replace("breadth = 3\n", "");
        let error = Declaration::parse(&text).expect_err("missing breadth");
        assert!(error.to_string().contains("breadth"), "{error}");
    }

    #[test]
    fn alternatives_follow_the_primary_cli() {
        let text = format!(
            "{}\n[[agent.planner.alternative]]\ndriver = \"codex\"\ncommand = [\"codex\"]\n\
             env = {{ A = \"1\" }}\n",
            Declaration::TEMPLATE
        );
        let declaration = Declaration::parse(&text).expect("parses");
        let clis = declaration.agent["planner"].clis();
        assert_eq!(clis.len(), 2);
        assert_eq!(clis[0].driver, DriverKind::ClaudeCode);
        assert_eq!(clis[1].driver, DriverKind::Codex);
        assert_eq!(clis[1].env["A"], "1");
        assert_eq!(declaration.agent["executor"].clis().len(), 1);

        let empty = format!(
            "{}\n[[agent.planner.alternative]]\ndriver = \"codex\"\ncommand = []\n",
            Declaration::TEMPLATE
        );
        let error = Declaration::parse(&empty).expect_err("empty command");
        assert!(error.to_string().contains("`command` is empty"), "{error}");
    }

    fn with_timeouts(agent: &str, alternative: &str) -> Result<Declaration, DeclarationError> {
        Declaration::parse(&format!(
            "[agent.a]\nrole = \"planning\"\ndriver = \"generic\"\ncommand = [\"x\"]\n\
             replicas = 1\n{agent}\n\
             [[agent.a.alternative]]\ndriver = \"generic\"\ncommand = [\"y\"]\n{alternative}\n"
        ))
    }

    #[test]
    fn timeouts_are_parsed_and_bad_ones_rejected() {
        let template = Declaration::parse(Declaration::TEMPLATE).expect("parses");
        assert!(template.agent.values().all(|a| a.timeout.is_none()));

        let timeouts = |agent: &str, alternative: &str| -> Vec<Option<Duration>> {
            with_timeouts(agent, alternative).expect("parses").agent["a"]
                .clis()
                .iter()
                .map(|cli| cli.timeout.as_ref().map(|t| t.duration().expect("valid")))
                .collect()
        };
        let secs = Duration::from_secs;
        assert_eq!(timeouts("timeout = 90", ""), [Some(secs(90)); 2]);
        assert_eq!(
            timeouts("timeout = \"1h 30m\"", "timeout = \"45s\""),
            [Some(secs(5400)), Some(secs(45))]
        );
        assert_eq!(timeouts("", "timeout = 5"), [None, Some(secs(5))]);

        for (agent, alternative, reason) in [
            ("timeout = 0", "", "not a positive duration"),
            ("timeout = -5", "", "not a positive duration"),
            ("timeout = \"0s\"", "", "not a positive duration"),
            ("timeout = \"soon\"", "", "not a duration"),
            ("", "timeout = \"\"", "not a duration"),
        ] {
            let error = with_timeouts(agent, alternative).expect_err("bad timeout");
            assert!(
                matches!(&error, DeclarationError::Timeout { agent, .. } if agent == "a"),
                "{agent} {alternative}: {error:?}"
            );
            assert!(error.to_string().contains(reason), "{error}");
        }
        let error = with_timeouts("timeout = 1.5", "").expect_err("float");
        assert!(matches!(error, DeclarationError::Parse { .. }), "{error:?}");
    }
}
