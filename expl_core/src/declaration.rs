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
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::DriverKind;
use crate::event::EventId;
use crate::project::Project;
use crate::role::Role;

mod routing;

pub(crate) use routing::Route;
pub use routing::{RouterDeclaration, RoutingPolicy};

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
    /// How what an agent produces is taken in.
    #[serde(default)]
    pub workspace: WorkspaceDeclaration,
    /// Named agents.
    pub agent: BTreeMap<String, AgentDeclaration>,
    /// Named routers, whose targets are agents or other routers.
    #[serde(default)]
    pub router: BTreeMap<String, RouterDeclaration>,
    /// Where to route the events of each explicitly assigned role.
    #[serde(default)]
    pub routing: BTreeMap<Role, String>,
}

/// `[workspace]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeclaration {
    /// Git pathspecs of the paths a change step's commit keeps as they
    /// were where the step started. The agent sees these paths in its own
    /// clone and may change them there; the change stays in the clone.
    #[serde(default)]
    pub frozen: Vec<String>,
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
    /// Maximum concurrent reactions of this agent across all its processes.
    #[serde(default)]
    pub concurrency: Option<NonZeroUsize>,
    /// Share of its role's events. Agents of one role that declare a
    /// weight split that role's events between them in proportion — each
    /// event goes to exactly one of them — instead of each reacting to
    /// every event. Agents without a weight react to every event of their
    /// role. Use it to route one role across agents with different
    /// instructions (e.g. a mainstream and a contrarian exploration). An
    /// event's recipient is recorded when it is first routed, so changed
    /// weights only route the events that come after.
    #[serde(default)]
    pub weight: Option<u32>,
    /// Exploration only: maximum steps proposed per reaction.
    #[serde(default)]
    pub breadth: Option<usize>,
    /// Exploration only: also explore ahead of steps not carried out yet,
    /// assuming they succeed, when revisiting starting points — the map
    /// grows deep before anything runs.
    #[serde(default)]
    pub speculate: Option<bool>,
    /// Exploration only: seconds an idle process waits after its last
    /// exploration before it revisits a random starting point. Without it,
    /// an exploration reacts to events only and never revisits.
    #[serde(default)]
    pub revisit_cooldown: Option<u64>,
    /// Exploration only: what one exploration looks at — `"node"` (the
    /// default) explores from the node an event names; `"graph"` surveys the
    /// whole graph at once, for what no single node would suggest.
    #[serde(default)]
    pub scope: Option<Scope>,
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

/// What one exploration looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The node an event names (or a goal, for counter-exploration).
    #[default]
    Node,
    /// The whole graph at once.
    Graph,
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
# `expl up` for `replicas` of each assigned agent). An agent reacts to the graph
# events of its role:
#   exploration  — goal.added, node.evaluated, origin.advanced → proposes hypotheses (steps);
#                  when idle, every `revisit_cooldown` seconds it explores again
#                  from a random starting point (node.revisited)
#   planning     — node.proposed/evaluated/failed, goal.retired, origin.advanced → paths and priorities
#   verification — node.ready                      → verifies the step: commits on its branch, or a finding
#   evaluation   — node.completed, origin.advanced → whether it held, verdicts against goals
#
# Optional `concurrency` limits simultaneous reactions across an agent's
# workers, including extra processes started by `expl run`. To route each
# role's events to one agent, set its target in `[routing]`. A target can be
# an agent or a `[router.<name>]` with `targets`, `policy` and `concurrency`.
# Each router shares its concurrency limit across its assigned descendants.
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

# The default branch. Merge the hypotheses you choose into it with git;
# running processes notice it moved, the origin follows, and exploration
# starts again from there.
trunk = "main"

# Every agent works in a private clone of the repository, with its whole
# history, and expl takes in what it leaves there. `frozen` lists git
# pathspecs (`*.md` matches Markdown at any depth) that a step's commit
# keeps as they were where the step started.
[workspace]
frozen = []

[agent.explorer]
role = "exploration"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 1
concurrency = 1
breadth = 3
revisit_cooldown = 600          # idle: explore again from a random node every 10 minutes

[agent.planner]
role = "planning"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 1
concurrency = 1

[agent.verifier]
role = "verification"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 2
concurrency = 2

[agent.evaluator]
role = "evaluation"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 1
concurrency = 1
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
        if self.workspace.frozen.iter().any(|p| p.trim().is_empty()) {
            return Err(DeclarationError::Invalid(
                "`workspace.frozen` holds an empty pathspec".to_owned(),
            ));
        }
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
            if agent.weight == Some(0) {
                return invalid(
                    name,
                    "`weight` must be >= 1 (leave it out to receive every event)",
                );
            }
            match agent.role {
                Role::Exploration => {
                    if agent.breadth.is_none_or(|b| b == 0) {
                        return invalid(name, "exploration needs `breadth` >= 1");
                    }
                }
                Role::Planning | Role::Verification | Role::Evaluation => {
                    if agent.breadth.is_some()
                        || agent.speculate.is_some()
                        || agent.revisit_cooldown.is_some()
                        || agent.scope.is_some()
                    {
                        return invalid(
                            name,
                            "`breadth`, `revisit_cooldown`, `scope` and `speculate` apply to \
                             exploration agents only",
                        );
                    }
                }
            }
        }
        self.validate_routing()
    }

    /// When the agent named `agent` shares its role's events by weight,
    /// the agent `event` is routed to (possibly `agent` itself); `None`
    /// when `agent` declares no weight, i.e. it reacts to every event. The
    /// choice depends only on the event and the declaration; processes that
    /// loaded different weights may propose differently, so it is only a
    /// proposal for [`Inbox::assign`](crate::event::Inbox::assign), whose
    /// recorded assignment every process obeys.
    #[must_use]
    pub fn route(&self, agent: &str, event: EventId) -> Option<&str> {
        let role = self.agent.get(agent)?.role;
        self.agent.get(agent)?.weight?;
        let shares: Vec<(&str, u64)> = self
            .agent
            .iter()
            .filter(|(_, a)| a.role == role)
            .filter_map(|(name, a)| a.weight.map(|w| (name.as_str(), u64::from(w))))
            .collect();
        let total: u64 = shares.iter().map(|(_, w)| w).sum();
        let mut point = spread(event.0 as u64) % total;
        for (name, weight) in shares {
            if point < weight {
                return Some(name);
            }
            point -= weight;
        }
        None
    }

    /// Whether `other` shares `agent`'s role by weight in this declaration:
    /// an assignment to an agent that does not (one since removed, or no
    /// longer weighted) would never be handled.
    #[must_use]
    pub fn shares_with(&self, agent: &str, other: &str) -> bool {
        match (self.agent.get(agent), self.agent.get(other)) {
            (Some(agent), Some(other)) => agent.role == other.role && other.weight.is_some(),
            _ => false,
        }
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

/// `SplitMix64`: consecutive event ids land far apart, so routing by weight
/// does not alternate in lockstep with the event log.
fn spread(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_is_a_valid_declaration() {
        let declaration = Declaration::parse(Declaration::TEMPLATE).expect("template parses");
        assert_eq!(declaration.agent.len(), 4);
        assert_eq!(declaration.agent["verifier"].role, Role::Verification);
        assert_eq!(declaration.agent["verifier"].replicas, 2);
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
    fn weighted_agents_split_their_role_by_weight() {
        let text = format!(
            "{}\n[agent.contrarian]\nrole = \"exploration\"\ndriver = \"generic\"\n\
             command = [\"x\"]\nreplicas = 1\nbreadth = 1\nweight = 1\n",
            Declaration::TEMPLATE.replace("breadth = 3\n", "breadth = 3\nweight = 3\n")
        );
        let declaration = Declaration::parse(&text).expect("parses");
        let mut counts = BTreeMap::new();
        for id in 1..=4000 {
            let explorer = declaration.route("explorer", EventId(id));
            let contrarian = declaration.route("contrarian", EventId(id));
            assert_eq!(explorer, contrarian, "every agent agrees on e{id}");
            *counts.entry(explorer.expect("routed")).or_insert(0) += 1;
        }
        assert!((2800..3200).contains(&counts["explorer"]), "{counts:?}");
        assert!((800..1200).contains(&counts["contrarian"]), "{counts:?}");
        // Unweighted agents react to everything of their role.
        assert_eq!(declaration.route("planner", EventId(1)), None);
        assert!(declaration.shares_with("explorer", "contrarian"));
        assert!(!declaration.shares_with("explorer", "removed"));
        assert!(!declaration.shares_with("explorer", "planner"));

        let zero = Declaration::TEMPLATE.replace("breadth = 3\n", "breadth = 3\nweight = 0\n");
        let error = Declaration::parse(&zero).expect_err("zero weight");
        assert!(
            error.to_string().contains("`weight` must be >= 1"),
            "{error}"
        );
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
        assert_eq!(declaration.agent["verifier"].clis().len(), 1);

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
