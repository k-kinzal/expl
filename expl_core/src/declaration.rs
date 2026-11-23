//! The project declaration in `expl.toml`: event consumers and routing.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::DriverKind;
use crate::event::EventKind;
use crate::handler::Handler;
use crate::project::Project;

pub use expl_runtime::command::{CliDeclaration, Timeout, TimeoutError};
use expl_runtime::control::{ControlError, NamedAgent};
#[cfg(test)]
pub(crate) use expl_runtime::registration::Route;
use expl_runtime::registration::{
    ConsumerRegistration, IdleCommand, Registration, TaskRegistration,
};
pub use expl_runtime::registration::{RouterDeclaration, RoutingPolicy};

/// How often the trunk is looked at, in seconds.
const FOLLOW_SECONDS: u64 = 2;

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
    /// No consumer has this name.
    #[error("no consumer named `{0}` in expl.toml")]
    UnknownConsumer(String),
    /// A consumer names daemon agents, and the project runs under none.
    #[error(
        "consumer `{0}` names daemon agents (`agents`), which only a daemon defines: run the \
         project under one (`expl daemon`), or declare `driver` and `command`"
    )]
    NoDaemon(String),
    /// A consumer names an agent its daemon does not define.
    #[error("consumer `{consumer}`: the daemon defines no agent named `{agent}` (daemon.toml)")]
    UnknownAgent {
        /// The consumer.
        consumer: String,
        /// The name it uses.
        agent: String,
    },
    /// The daemon's agent control could not be read.
    #[error(transparent)]
    Control(Box<ControlError>),
    /// A consumer's `timeout` (or one of its alternatives') is zero or not a
    /// duration.
    #[error("consumer `{consumer}`: invalid `timeout`: {source}")]
    Timeout {
        /// The consumer declaring it.
        consumer: String,
        /// Why the value is unusable.
        #[source]
        source: TimeoutError,
    },
}

impl From<ControlError> for DeclarationError {
    fn from(error: ControlError) -> Self {
        Self::Control(Box::new(error))
    }
}

/// The whole `expl.toml`, with legacy declarations normalized to consumers.
#[derive(Debug, Clone, Serialize)]
pub struct Declaration {
    /// The default branch expl follows.
    pub trunk: Option<String>,
    /// How changes are taken in.
    pub workspace: WorkspaceDeclaration,
    /// OpenTelemetry export for this project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryDeclaration>,
    /// The pool whose shared agent limit this project counts against under
    /// a daemon (default: one of its own, named after the project).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// Named event consumers.
    pub consumer: BTreeMap<String, ConsumerDeclaration>,
    /// Named routers, whose targets are consumers or other routers.
    pub router: BTreeMap<String, RouterDeclaration>,
    /// Name of each consumer group and its routing target.
    pub routing: BTreeMap<String, String>,
    /// What the runtime delivers by: the consumers' subscriptions, groups
    /// and limits, and the routers, as loaded.
    #[serde(skip)]
    registration: Registration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclarationFile {
    #[serde(default)]
    trunk: Option<String>,
    #[serde(default)]
    workspace: WorkspaceDeclaration,
    #[serde(default)]
    telemetry: Option<TelemetryDeclaration>,
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    consumer: BTreeMap<String, ConsumerDeclaration>,
    #[serde(default)]
    router: BTreeMap<String, RouterDeclaration>,
    #[serde(default)]
    routing: BTreeMap<String, String>,
    #[serde(default)]
    agent: BTreeMap<String, ConsumerDeclaration>,
}

impl DeclarationFile {
    fn into_declaration(mut self) -> Result<Declaration, DeclarationError> {
        if !self.agent.is_empty() && !self.consumer.is_empty() {
            return Err(DeclarationError::Invalid(
                "`[agent]` and `[consumer]` cannot be used together".to_owned(),
            ));
        }
        for (name, consumer) in &self.consumer {
            if consumer.weight.is_some() {
                return Err(DeclarationError::Invalid(format!(
                    "consumer `{name}`: set weights on a router"
                )));
            }
        }
        if !self.agent.is_empty() {
            if let Some(root) = self.routing.remove("execution")
                && self
                    .routing
                    .insert("verification".to_owned(), root)
                    .is_some()
            {
                return Err(DeclarationError::Invalid(
                    "legacy routing declares both `execution` and `verification`".to_owned(),
                ));
            }
            for name in self.routing.keys() {
                if !Handler::ALL.iter().any(|handler| handler.as_str() == name) {
                    return Err(DeclarationError::Invalid(format!(
                        "unknown legacy routing key `{name}`"
                    )));
                }
            }
            for agent in self.agent.values_mut() {
                agent.on = agent.handler.events().to_vec();
                let group = agent.handler.as_str();
                if self.routing.contains_key(group) {
                    agent.group = Some(group.to_owned());
                }
            }
            self.route_by_weight();
            self.consumer = self.agent;
        }
        let registration = Registration {
            consumer: self
                .consumer
                .iter()
                .map(|(name, consumer)| (name.clone(), consumer.subscription()))
                .collect(),
            router: self.router.clone(),
            routing: self.routing.clone(),
            pool: self.pool.clone(),
            ..Registration::default()
        };
        let declaration = Declaration {
            trunk: self.trunk,
            workspace: self.workspace,
            telemetry: self.telemetry,
            pool: self.pool,
            consumer: self.consumer,
            router: self.router,
            routing: self.routing,
            registration,
        };
        declaration.validate()?;
        Ok(declaration)
    }

    /// Agents that share their role's events by an inline `weight` become a
    /// weighted router over those agents, under the role's name as their
    /// delivery group: the same split, and the recipients recorded for the
    /// role so far still hold. A role that is routed explicitly keeps that
    /// routing, and agents without a weight keep reacting to every event.
    fn route_by_weight(&mut self) {
        for handler in Handler::ALL {
            let role = handler.as_str();
            if self.routing.contains_key(role) {
                continue;
            }
            let weights: BTreeMap<String, u32> = self
                .agent
                .iter()
                .filter(|(_, agent)| agent.handler == handler)
                .filter_map(|(name, agent)| agent.weight.map(|weight| (name.clone(), weight)))
                .collect();
            if weights.is_empty() {
                continue;
            }
            for name in weights.keys() {
                if let Some(agent) = self.agent.get_mut(name) {
                    agent.group = Some(role.to_owned());
                }
            }
            let router = format!("{role}:weight");
            self.router.insert(
                router.clone(),
                RouterDeclaration {
                    targets: weights.keys().cloned().collect(),
                    policy: RoutingPolicy::Weighted,
                    // The split is all the router is for: no limit of its own.
                    concurrency: NonZeroUsize::MAX,
                    weights,
                },
            );
            self.routing.insert(role.to_owned(), router);
        }
    }
}

/// `[telemetry]` configures the project's OTLP HTTP export destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryDeclaration {
    /// Base URL of the OTLP HTTP receiver, before `/v1/traces` or `/v1/metrics`.
    pub endpoint: String,
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
    /// Optional Git remote to follow and publish committed hypotheses to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

/// One `[consumer.<name>]`: when to react, what to do, and how to run it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerDeclaration {
    /// The graph events that trigger this consumer.
    #[serde(default)]
    pub on: Vec<EventKind>,
    /// How to handle a delivered event.
    #[serde(alias = "role")]
    pub handler: Handler,
    /// Optional shared delivery group, declared under `[routing]`.
    #[serde(default)]
    pub group: Option<String>,
    /// Which driver translates for its CLI. Declared together with
    /// `command`, unless the consumer names daemon `agents` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<DriverKind>,
    /// Program and leading arguments, e.g. `["claude"]`.
    #[serde(default)]
    pub command: Vec<String>,
    /// Agents of the daemon the project runs under, by the names its
    /// operator gave them (`agents = ["claude", "cursor"]`), instead of a
    /// `driver` and `command` of the consumer's own. Their CLIs take turns
    /// in this order, before any `alternative`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// Extra arguments appended after the driver's own flags.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the CLI.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Project-specific guidance appended to the handler prompt.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Processes `expl up` keeps running.
    pub replicas: usize,
    /// Maximum concurrent reactions across this consumer's workers.
    #[serde(default)]
    pub concurrency: Option<NonZeroUsize>,
    // Legacy inline weighted fan-out. New declarations use a weighted router.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    weight: Option<u32>,
    /// Exploration only: maximum steps proposed per reaction.
    #[serde(default)]
    pub breadth: Option<usize>,
    /// Exploration only: also explore ahead of steps not carried out yet,
    /// assuming they succeed, when revisiting starting points — the map
    /// grows deep before anything runs.
    #[serde(default)]
    pub speculate: Option<bool>,
    /// Exploration only: seconds of idle time before a worker publishes
    /// `node.revisited`. Applies when this consumer subscribes to that event.
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
    /// Further CLIs that can supply the handler response (`[[consumer.<name>.alternative]]`).
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

impl ConsumerDeclaration {
    /// What the runtime needs to deliver this consumer's events: its
    /// subscription, group and limits.
    fn subscription(&self) -> ConsumerRegistration {
        ConsumerRegistration {
            on: self
                .on
                .iter()
                .map(|kind| kind.as_str().to_owned())
                .collect(),
            group: self.group.clone(),
            replicas: self.replicas,
            concurrency: self.concurrency,
            exclusive: self.handler.is_exclusive(),
            coalesce: self.handler.coalesces(),
            attributes: BTreeMap::from([(
                "expl.handler".to_owned(),
                self.handler.as_str().to_owned(),
            )]),
            ..ConsumerRegistration::default()
        }
    }

    /// Inline routing weight of a legacy consumer, if declared.
    #[must_use]
    pub fn legacy_weight(&self) -> Option<u32> {
        self.weight
    }

    /// Every CLI the consumer declares itself, primary first, each with
    /// the `timeout` that bounds it (its own, else the consumer's). A
    /// consumer that names daemon `agents` declares only its alternatives;
    /// [`Self::clis_among`] puts the named agents before them.
    #[must_use]
    pub fn clis(&self) -> Vec<CliDeclaration> {
        let primary = self.driver.map(|driver| CliDeclaration {
            driver,
            command: self.command.clone(),
            args: self.args.clone(),
            env: BTreeMap::new(),
            timeout: self.timeout.clone(),
        });
        let alternatives = self.alternative.iter().map(|cli| CliDeclaration {
            timeout: cli.timeout.clone().or_else(|| self.timeout.clone()),
            ..cli.clone()
        });
        primary.into_iter().chain(alternatives).collect()
    }

    /// Every CLI of the consumer under a daemon whose operator defined
    /// `named`: the CLIs of each agent it names, in order, then its own.
    /// The consumer's `timeout` bounds a named CLI that has none.
    ///
    /// # Errors
    ///
    /// The first name the daemon does not define.
    pub fn clis_among(
        &self,
        named: &BTreeMap<String, NamedAgent>,
    ) -> Result<Vec<CliDeclaration>, String> {
        let mut clis = Vec::new();
        for name in &self.agents {
            let agent = named.get(name).ok_or_else(|| name.clone())?;
            clis.extend(agent.clis().into_iter().map(|cli| CliDeclaration {
                timeout: cli.timeout.clone().or_else(|| self.timeout.clone()),
                ..cli
            }));
        }
        clis.extend(self.clis());
        Ok(clis)
    }
}

impl Declaration {
    /// The declaration `expl init` writes.
    pub const TEMPLATE: &'static str = r#"# expl declaration. See README.md and `expl --help`.
#
# Each [consumer.<name>] subscribes to the events listed in `on` and runs
# its `handler` when one arrives. `expl run <name>` starts a worker and
# `expl up` starts the declared number of `replicas`.
#
# Consumers have separate inboxes unless they share a delivery `group`.
# In [routing], assign the group to a consumer or [router.<name>].
# Routers choose among targets subscribed to the event. Optional
# `concurrency` limits simultaneous reactions across workers; a router's
# limit is shared across its assigned consumers.
#
# `driver` selects how the CLI is spoken to:
#   claude_code  — `claude --print --output-format json --json-schema ...`
#   codex        — `codex exec --output-schema ... --output-last-message ...`
#   cursor_agent — `cursor-agent --print --output-format json ...`
#   generic      — any command: prompt on stdin, JSON response on stdout
#
# A consumer may use further CLIs with [[consumer.<name>.alternative]] (driver,
# command, args, env). Its invocations are then routed across all of them in
# turn, falling over to the next CLI when one fails.
#
# Under a daemon, a consumer may name agents its operator defined in the
# daemon's `daemon.toml` (`agents = ["<name>", ...]`) instead of a `driver`
# and `command` of its own, and a top-level `pool = "<name>"` says whose
# agent limit the project counts against.
#
# An optional `timeout` (seconds, or a duration such as "30m") bounds each
# CLI attempt: past it, the CLI is stopped and the attempt fails like any
# other. An alternative's own `timeout` overrides the consumer's.
#
# Agents run unattended inside private clones, so the defaults
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

[consumer.explorer]
on = ["goal.added", "node.evaluated", "node.failed", "origin.advanced", "node.revisited"]
handler = "exploration"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 1
concurrency = 1
breadth = 3
revisit_cooldown = 600          # idle: explore again from a random node every 10 minutes

[consumer.planner]
on = ["node.proposed", "node.evaluated", "node.failed", "goal.retired", "origin.advanced"]
handler = "planning"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 1
concurrency = 1

[consumer.verifier]
on = ["node.ready"]
handler = "verification"
driver = "claude_code"
command = ["claude"]
args = ["--permission-mode", "bypassPermissions"]
replicas = 2
concurrency = 2

[consumer.evaluator]
on = ["node.completed", "node.failed", "origin.advanced"]
handler = "evaluation"
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
        Self::parse_at(&text, &path.display().to_string())
    }

    /// Parse and validate declaration text.
    ///
    /// # Errors
    ///
    /// [`DeclarationError::Parse`] or [`DeclarationError::Invalid`].
    pub fn parse(text: &str) -> Result<Self, DeclarationError> {
        Self::parse_at(text, Project::DECLARATION_FILE)
    }

    fn parse_at(text: &str, path: &str) -> Result<Self, DeclarationError> {
        let file: DeclarationFile =
            toml::from_str(text).map_err(|source| DeclarationError::Parse {
                path: path.to_owned(),
                source,
            })?;
        file.into_declaration()
    }

    fn validate(&self) -> Result<(), DeclarationError> {
        let invalid = |name: &str, what: &str| {
            Err(DeclarationError::Invalid(format!(
                "consumer `{name}`: {what}"
            )))
        };
        if let Some(remote) = &self.workspace.remote
            && (remote.starts_with('-')
                || remote.is_empty()
                || remote.bytes().any(|byte| byte.is_ascii_whitespace()))
        {
            return Err(DeclarationError::Invalid(
                "`workspace.remote` must name a configured Git remote".to_owned(),
            ));
        }
        if self
            .pool
            .as_ref()
            .is_some_and(|pool| pool.trim().is_empty())
        {
            return Err(DeclarationError::Invalid("`pool` is empty".to_owned()));
        }
        if self.workspace.frozen.iter().any(|p| p.trim().is_empty()) {
            return Err(DeclarationError::Invalid(
                "`workspace.frozen` holds an empty pathspec".to_owned(),
            ));
        }
        for (name, consumer) in &self.consumer {
            if consumer.on.is_empty() {
                return invalid(name, "`on` must list at least one event");
            }
            for (i, event) in consumer.on.iter().enumerate() {
                if !consumer.handler.events().contains(event) {
                    return invalid(
                        name,
                        &format!("`{}` does not handle `{event}`", consumer.handler),
                    );
                }
                if consumer.on[..i].contains(event) {
                    return invalid(name, &format!("duplicate event `{event}` in `on`"));
                }
            }
            if let Some(group) = &consumer.group
                && !self.routing.contains_key(group)
            {
                return invalid(name, &format!("unknown delivery group `{group}`"));
            }
            let inline = consumer.driver.is_some() || !consumer.command.is_empty();
            if inline && !consumer.agents.is_empty() {
                return invalid(
                    name,
                    "declare `driver` and `command`, or name daemon agents with `agents`, not both",
                );
            }
            if !inline && consumer.agents.is_empty() {
                return invalid(name, "needs `driver` and `command`, or `agents`");
            }
            if inline && consumer.driver.is_none() {
                return invalid(name, "`command` needs a `driver`");
            }
            if !inline && !consumer.args.is_empty() {
                return invalid(
                    name,
                    "`args` belong to a `command`; a named agent brings its own",
                );
            }
            if consumer.agents.iter().any(|agent| agent.trim().is_empty()) {
                return invalid(name, "`agents` holds an empty name");
            }
            if consumer
                .clis()
                .iter()
                .any(|cli| cli.command.first().is_none_or(|p| p.trim().is_empty()))
            {
                return invalid(name, "`command` is empty");
            }
            for timeout in consumer
                .clis()
                .iter()
                .filter_map(|cli| cli.timeout.as_ref())
            {
                timeout
                    .duration()
                    .map_err(|source| DeclarationError::Timeout {
                        consumer: name.clone(),
                        source,
                    })?;
            }
            if consumer.weight == Some(0) {
                return invalid(
                    name,
                    "`weight` must be >= 1 (leave it out to receive every event)",
                );
            }
            match consumer.handler {
                Handler::Exploration => {
                    if consumer.breadth.is_none_or(|b| b == 0) {
                        return invalid(name, "exploration needs `breadth` >= 1");
                    }
                }
                Handler::Planning | Handler::Verification | Handler::Evaluation => {
                    if consumer.breadth.is_some()
                        || consumer.speculate.is_some()
                        || consumer.revisit_cooldown.is_some()
                        || consumer.scope.is_some()
                    {
                        return invalid(
                            name,
                            "`breadth`, `revisit_cooldown`, `scope` and `speculate` apply to \
                             the exploration handler only",
                        );
                    }
                }
            }
        }
        self.registration
            .validate()
            .map_err(|error| DeclarationError::Invalid(error.to_string()))
    }

    /// The consumers' subscriptions, groups and limits, and the routers,
    /// as the runtime delivers by them.
    #[must_use]
    pub fn registration(&self) -> &Registration {
        &self.registration
    }

    /// Everything this explorer registers with the framework for the
    /// project at `root`, operated through `program` (the `expl` binary):
    /// each consumer with the command that makes one reaction of its
    /// handler, the command an idle exploration revisits a starting point
    /// with, and the task that follows the trunk. The commands run on the
    /// leader, beside the graph.
    #[must_use]
    pub fn registration_for(&self, program: &Path, root: &Path) -> Registration {
        let command = |arguments: &[&str]| -> Vec<String> {
            [
                program.to_string_lossy().into_owned(),
                "-C".to_owned(),
                root.to_string_lossy().into_owned(),
            ]
            .into_iter()
            .chain(arguments.iter().map(|&argument| argument.to_owned()))
            .collect()
        };
        let mut registration = self.registration.clone();
        for (name, consumer) in &mut registration.consumer {
            consumer.run = command(&["react", name]);
            let declared = &self.consumer[name];
            if let Some(cooldown) = declared.revisit_cooldown
                && declared.on.contains(&EventKind::NodeRevisited)
            {
                consumer.idle = Some(IdleCommand {
                    after: cooldown,
                    run: command(&["revisit", name]),
                });
            }
        }
        registration.task.insert(
            "follow".to_owned(),
            TaskRegistration {
                every: FOLLOW_SECONDS,
                run: command(&["follow"]),
            },
        );
        registration
    }

    /// Whether this consumer is reachable through its delivery group's router.
    #[must_use]
    pub fn is_assigned(&self, name: &str) -> bool {
        self.registration.is_assigned(name)
    }

    #[cfg(test)]
    pub(crate) fn route_for(&self, name: &str) -> Option<Route<'_>> {
        self.registration.route_for(name)
    }

    /// Whether an assigned consumer handles this event with the given operation.
    #[must_use]
    pub fn handles(&self, handler: Handler, event: EventKind) -> bool {
        self.consumer.iter().any(|(name, consumer)| {
            consumer.handler == handler && consumer.on.contains(&event) && self.is_assigned(name)
        })
    }

    /// The consumer named `name`.
    ///
    /// # Errors
    ///
    /// [`DeclarationError::UnknownConsumer`].
    pub fn consumer(&self, name: &str) -> Result<&ConsumerDeclaration, DeclarationError> {
        self.consumer
            .get(name)
            .ok_or_else(|| DeclarationError::UnknownConsumer(name.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::event::EventId;

    #[test]
    fn template_is_a_valid_declaration() {
        let declaration = Declaration::parse(Declaration::TEMPLATE).expect("template parses");
        assert_eq!(declaration.consumer.len(), 4);
        assert_eq!(
            declaration.consumer["verifier"].handler,
            Handler::Verification
        );
        assert_eq!(declaration.consumer["verifier"].replicas, 2);
        assert_eq!(declaration.trunk.as_deref(), Some("main"));
        let none = Declaration::parse(&Declaration::template(None)).expect("parses");
        assert_eq!(none.trunk, None);
        let dev = Declaration::parse(&Declaration::template(Some("dev"))).expect("parses");
        assert_eq!(dev.trunk.as_deref(), Some("dev"));
    }

    #[test]
    fn exploration_settings_are_handler_specific() {
        let text = Declaration::TEMPLATE.replace(
            "handler = \"planning\"",
            "handler = \"planning\"\nbreadth = 2",
        );
        let error = Declaration::parse(&text).expect_err("breadth on planner");
        assert!(
            error.to_string().contains("the exploration handler only"),
            "{error}"
        );
        let text = Declaration::TEMPLATE.replace("breadth = 3\n", "");
        let error = Declaration::parse(&text).expect_err("missing breadth");
        assert!(error.to_string().contains("breadth"), "{error}");
    }

    #[test]
    fn subscriptions_must_name_distinct_events_supported_by_the_handler() {
        for (on, problem) in [
            ("", "`on` must list at least one event"),
            ("\"goal.added\"", "does not handle `goal.added`"),
            (
                "\"node.ready\", \"node.ready\"",
                "duplicate event `node.ready`",
            ),
        ] {
            let config =
                Declaration::TEMPLATE.replace("on = [\"node.ready\"]", &format!("on = [{on}]"));
            let error = Declaration::parse(&config).expect_err(problem);
            assert!(error.to_string().contains(problem), "{error}");
        }
    }

    #[test]
    fn legacy_explicit_routing_keeps_its_assignment_namespace() {
        let config = r#"
[agent.check]
role = "execution"
driver = "generic"
command = ["check"]
replicas = 1

[routing]
execution = "check"
"#;
        let declaration = Declaration::parse(config).expect("legacy declaration");
        assert_eq!(declaration.consumer["check"].on, [EventKind::NodeReady]);
        let route = declaration.route_for("check").expect("route");
        assert_eq!(route.group(), "verification");
        assert_eq!(route.select("node.ready", EventId(1)), "check");
    }

    #[test]
    fn weighted_agents_split_their_role_by_weight() {
        let legacy = Declaration::TEMPLATE
            .lines()
            .filter(|line| !line.starts_with("on = "))
            .collect::<Vec<_>>()
            .join("\n")
            .replace("[consumer.", "[agent.")
            .replace("handler = ", "role = ");
        let text = format!(
            "{}\n[agent.contrarian]\nrole = \"exploration\"\ndriver = \"generic\"\n\
             command = [\"x\"]\nreplicas = 1\nbreadth = 1\nweight = 1\n",
            legacy.replace("breadth = 3\n", "breadth = 3\nweight = 3\n")
        );
        let declaration = Declaration::parse(&text).expect("parses");
        // The weighted agents are routed as one group, named for their role.
        let explorer = declaration.route_for("explorer").expect("route");
        let contrarian = declaration.route_for("contrarian").expect("route");
        assert_eq!(explorer.group(), "exploration");
        let mut counts = BTreeMap::new();
        for id in 1..=4000 {
            let chosen = explorer.select("goal.added", EventId(id));
            assert_eq!(
                chosen,
                contrarian.select("goal.added", EventId(id)),
                "every agent agrees on e{id}"
            );
            *counts.entry(chosen.to_owned()).or_insert(0) += 1;
        }
        assert!((2800..3200).contains(&counts["explorer"]), "{counts:?}");
        assert!((800..1200).contains(&counts["contrarian"]), "{counts:?}");
        // The split is all the router does: it limits nothing.
        assert!(
            explorer
                .groups("explorer")
                .all(|(_, limit)| limit == NonZeroUsize::MAX)
        );
        // Unweighted agents react to everything of their role.
        assert!(declaration.route_for("planner").is_none());
        assert!(declaration.is_assigned("planner"));

        let zero = legacy.replace("breadth = 3\n", "breadth = 3\nweight = 0\n");
        let error = Declaration::parse(&zero).expect_err("zero weight");
        assert!(
            error.to_string().contains("`weight` must be >= 1"),
            "{error}"
        );
    }

    #[test]
    fn a_consumer_declares_its_own_cli_or_names_daemon_agents() {
        let named = Declaration::TEMPLATE
            .replace(
                "handler = \"planning\"\ndriver = \"claude_code\"\ncommand = [\"claude\"]\n\
                 args = [\"--permission-mode\", \"bypassPermissions\"]",
                "handler = \"planning\"\nagents = [\"claude\", \"cursor\"]\ntimeout = \"20m\"",
            )
            .replace("trunk = \"main\"", "trunk = \"main\"\npool = \"general\"");
        let text = format!(
            "{named}\n[[consumer.planner.alternative]]\ndriver = \"codex\"\ncommand = [\"codex\"]\n"
        );
        let declaration = Declaration::parse(&text).expect("parses");
        assert_eq!(declaration.pool.as_deref(), Some("general"));
        assert_eq!(declaration.registration().pool.as_deref(), Some("general"));
        let planner = &declaration.consumer["planner"];
        assert_eq!(planner.agents, ["claude", "cursor"]);
        assert_eq!(planner.driver, None);
        // By itself it declares only its alternative.
        assert_eq!(planner.clis().len(), 1);

        let control = expl_runtime::control::Control::parse(
            "[agent.claude]\ndriver = \"claude_code\"\ncommand = [\"claude\"]\n\
             args = [\"--model\", \"claude-opus-5-5\"]\n\n\
             [agent.cursor]\ndriver = \"cursor_agent\"\ncommand = [\"cursor-agent\"]\ntimeout = 60\n",
        )
        .expect("control");
        let clis = planner.clis_among(&control.agent).expect("named");
        let drivers: Vec<_> = clis.iter().map(|cli| cli.driver).collect();
        assert_eq!(
            drivers,
            [
                DriverKind::ClaudeCode,
                DriverKind::CursorAgent,
                DriverKind::Codex
            ]
        );
        assert_eq!(clis[0].args, ["--model", "claude-opus-5-5"]);
        // The consumer's timeout bounds a named CLI that has none.
        let timeouts: Vec<_> = clis
            .iter()
            .map(|cli| cli.timeout.as_ref().map(|t| t.duration().expect("valid")))
            .collect();
        assert_eq!(
            timeouts,
            [
                Some(Duration::from_secs(1200)),
                Some(Duration::from_secs(60)),
                Some(Duration::from_secs(1200))
            ]
        );
        control.agent.get("claude").expect("defined");
        let mut fewer = control.agent.clone();
        fewer.remove("cursor");
        assert_eq!(planner.clis_among(&fewer).expect_err("undefined"), "cursor");

        for (from, to, problem) in [
            (
                "agents = [\"claude\", \"cursor\"]",
                "agents = [\"claude\"]\ndriver = \"generic\"\ncommand = [\"x\"]",
                "not both",
            ),
            (
                "agents = [\"claude\", \"cursor\"]",
                "",
                "needs `driver` and `command`, or `agents`",
            ),
            (
                "agents = [\"claude\", \"cursor\"]",
                "command = [\"x\"]",
                "`command` needs a `driver`",
            ),
            (
                "agents = [\"claude\", \"cursor\"]",
                "agents = [\"claude\"]\nargs = [\"--fast\"]",
                "`args` belong to a `command`",
            ),
            (
                "agents = [\"claude\", \"cursor\"]",
                "agents = [\"\"]",
                "empty name",
            ),
            ("pool = \"general\"", "pool = \" \"", "`pool` is empty"),
        ] {
            let error = Declaration::parse(&text.replacen(from, to, 1)).expect_err(problem);
            assert!(error.to_string().contains(problem), "{error}");
        }
    }

    #[test]
    fn alternatives_follow_the_primary_cli() {
        let text = format!(
            "{}\n[[consumer.planner.alternative]]\ndriver = \"codex\"\ncommand = [\"codex\"]\n\
             env = {{ A = \"1\" }}\n",
            Declaration::TEMPLATE
        );
        let declaration = Declaration::parse(&text).expect("parses");
        let clis = declaration.consumer["planner"].clis();
        assert_eq!(clis.len(), 2);
        assert_eq!(clis[0].driver, DriverKind::ClaudeCode);
        assert_eq!(clis[1].driver, DriverKind::Codex);
        assert_eq!(clis[1].env["A"], "1");
        assert_eq!(declaration.consumer["verifier"].clis().len(), 1);

        let empty = format!(
            "{}\n[[consumer.planner.alternative]]\ndriver = \"codex\"\ncommand = []\n",
            Declaration::TEMPLATE
        );
        let error = Declaration::parse(&empty).expect_err("empty command");
        assert!(error.to_string().contains("`command` is empty"), "{error}");
    }

    const ROUTERS: &str = r#"
[routing]
exploration = "all"
verification = "all"

[router.all]
targets = ["verifier", "explorers"]
policy = "first"
concurrency = 2

[router.explorers]
targets = ["explorer", "fringe"]
policy = "weighted"
weights = { explorer = 4 }
concurrency = 1

[consumer.fringe]
on = ["goal.added"]
handler = "exploration"
group = "exploration"
driver = "generic"
command = ["fringe"]
replicas = 1
breadth = 2
"#;

    fn template_with_groups() -> String {
        Declaration::TEMPLATE
            .replace(
                "handler = \"exploration\"",
                "handler = \"exploration\"\ngroup = \"exploration\"",
            )
            .replace(
                "handler = \"verification\"",
                "handler = \"verification\"\ngroup = \"verification\"",
            )
    }

    #[test]
    fn nested_routing_applies_policies_after_filtering_for_the_event_and_group() {
        let declaration = Declaration::parse(&format!("{}{ROUTERS}", template_with_groups()))
            .expect("declaration");
        let exploration = declaration.route_for("explorer").expect("route");
        let verification = declaration.route_for("verifier").expect("route");
        let fringes = (1..=4000)
            .filter(|id| exploration.select("goal.added", EventId(*id)) == "fringe")
            .count();
        assert!((700..900).contains(&fringes), "{fringes}");
        assert_eq!(verification.select("node.ready", EventId(1)), "verifier");
        for id in 1..=100 {
            assert_eq!(
                exploration.select("node.evaluated", EventId(id)),
                "explorer"
            );
        }
        assert_eq!(
            exploration
                .groups("explorer")
                .map(|(name, n)| (name, n.get()))
                .collect::<Vec<_>>(),
            [("all", 2), ("explorers", 1)]
        );
        assert_eq!(verification.groups("verifier").count(), 1);
    }

    #[test]
    fn routing_rejects_invalid_graphs_and_limits() {
        let base = format!("{}{ROUTERS}", template_with_groups());
        for (from, to, problem) in [
            (
                "\"verifier\", \"explorers\"",
                "\"missing\"",
                "unknown routing target",
            ),
            (
                "\"explorer\", \"fringe\"",
                "\"explorer\", \"fringe\", \"all\"",
                "router cycle",
            ),
            (
                "exploration = \"all\"",
                "exploration = \"verifier\"",
                "no consumer in this group",
            ),
            ("explorer = 4", "explorer = 0", "weights"),
            ("concurrency = 2", "concurrency = 0", "concurrency"),
        ] {
            let error = Declaration::parse(&base.replacen(from, to, 1)).expect_err(problem);
            assert!(error.to_string().contains(problem), "{error}");
        }
    }

    fn with_timeouts(agent: &str, alternative: &str) -> Result<Declaration, DeclarationError> {
        Declaration::parse(&format!(
            "[consumer.a]\non = [\"node.proposed\"]\nhandler = \"planning\"\ndriver = \"generic\"\ncommand = [\"x\"]\n\
             replicas = 1\n{agent}\n\
             [[consumer.a.alternative]]\ndriver = \"generic\"\ncommand = [\"y\"]\n{alternative}\n"
        ))
    }

    #[test]
    fn timeouts_are_parsed_and_bad_ones_rejected() {
        let template = Declaration::parse(Declaration::TEMPLATE).expect("parses");
        assert!(template.consumer.values().all(|a| a.timeout.is_none()));

        let timeouts = |agent: &str, alternative: &str| -> Vec<Option<Duration>> {
            with_timeouts(agent, alternative).expect("parses").consumer["a"]
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
                matches!(&error, DeclarationError::Timeout { consumer, .. } if consumer == "a"),
                "{agent} {alternative}: {error:?}"
            );
            assert!(error.to_string().contains(reason), "{error}");
        }
        let error = with_timeouts("timeout = 1.5", "").expect_err("float");
        assert!(matches!(error, DeclarationError::Parse { .. }), "{error:?}");
    }
}
