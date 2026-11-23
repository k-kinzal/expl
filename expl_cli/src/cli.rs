//! `clap` derive structures for the `expl` CLI.
//!
//! Resources follow iter's `<resource> <verb>` form. Processes are managed
//! with Docker's verbs, and the familiar short forms are top-level aliases:
//!
//! ```text
//! Setup:      expl init · expl doctor [--fix]
//! Graph:      expl goal add|ls|retire
//!             expl node ls|inspect|add|retry|prune|logs
//! Trunk:      expl origin ls   (the trunk itself moves with git)
//!             expl graph show · expl path ls · expl hypothesis ls
//!             expl event ls
//! Agents:     expl agent ls
//! Processes:  expl run <AGENT> [-d]
//!             expl up [AGENT...] · expl down
//!             expl process ls|inspect|logs|stop|kill|rm
//!             (aliases: expl ps | logs | stop | kill | rm)
//! ```

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use expl_core::{EventKind, NodeId, NodeKind, NodeStatus};

/// Top-level `expl` CLI definition.
#[derive(Debug, Parser)]
#[command(
    name = "expl",
    version,
    about = "Explore a problem space with agents that react to graph events",
    long_about = "expl explores a problem space as a graph of git branches. Every change to the \
                  graph is an event. Agents declared in expl.toml react to the events of their \
                  role: exploration proposes hypotheses — steps toward goals (and backward from them) — \
                  planning orders them into paths and sets priorities, verification tests ready \
                  hypotheses (as commits on their own branches, or as findings), and evaluation \
                  judges whether each was verified and how far it moves the goals. Agents run \
                  as processes, managed like containers: run as many as you \
                  like, and processes of one agent share its events, so independent work happens \
                  in parallel. Exploration never ends by itself: choosing among the hypotheses \
                  that satisfy a goal is yours.",
    after_help = "EXAMPLES:\n  \
                  expl init\n  \
                  expl goal add \"Support Codex\" --objective \"...\"\n  \
                  expl up                       # every agent, `replicas` processes each\n  \
                  expl ps\n  \
                  expl run verifier -d          # one more verifier\n  \
                  expl logs verifier-5 -f\n  \
                  expl hypothesis ls\n  \
                  git merge expl/node-7         # adopt it; running processes follow the trunk\n  \
                  expl origin ls                # what was adopted, and when\n  \
                  expl down"
)]
pub(crate) struct Cli {
    /// Run as if expl was started in this directory.
    #[arg(short = 'C', global = true, value_name = "DIR")]
    pub(crate) directory: Option<PathBuf>,

    /// Subcommand to dispatch.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Initialise expl in the current git repository.
    #[command(
        long_about = "Create .expl/ (database, worktrees, logs), write expl.toml if absent, \
                      exclude .expl/ from git, and record the origin node at the base commit."
    )]
    Init(InitArgs),
    /// Check the graph against git.
    #[command(
        long_about = "Report where the graph and git disagree, and exit non-zero if they do: \
                      completed or failed steps whose branch or head commit is missing or is not \
                      the branch tip; running steps nobody holds a lease for; origins \
                      whose commit no longer exists; node worktrees still registered for steps \
                      that are settled or unknown. With --fix, repair what is safe: orphaned \
                      running steps are unclaimed so they are delivered again, and stale \
                      worktree registrations are pruned (a worktree with changes is kept). \
                      The rest is left to you."
    )]
    Doctor(DoctorArgs),
    /// Manage goals.
    #[command(subcommand)]
    Goal(GoalCommand),
    /// Inspect and adjust nodes.
    #[command(subcommand)]
    Node(NodeCommand),
    /// Render the whole graph.
    #[command(subcommand)]
    Graph(GraphCommand),
    /// Show the latest plan's paths.
    #[command(subcommand)]
    Path(PathCommand),
    /// Show hypotheses evaluated as satisfying a goal.
    #[command(subcommand, alias = "candidate")]
    Hypothesis(HypothesisCommand),
    /// Show the trunk's lineage: every origin exploration started from.
    #[command(subcommand)]
    Origin(OriginCommand),
    /// Show the event log.
    #[command(subcommand)]
    Event(EventCommand),
    /// Show declared agents.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Manage processes.
    #[command(subcommand)]
    Process(ProcessCommand),
    /// Start a process of an agent.
    #[command(
        long_about = "Start a process of the declared agent. It leases the events of its role \
                      from the agent's inbox one at a time, reacts, and acknowledges them, until \
                      stopped. Processes of the same agent share its events, so starting more \
                      processes adds parallelism. With -d the process runs in the background and \
                      its name is printed."
    )]
    Run(RunArgs),
    /// Start processes until every agent has `replicas` running.
    Up(UpArgs),
    /// Stop every running process.
    Down(StopArgs),
    /// List processes (alias of `expl process ls`).
    Ps(PsArgs),
    /// Print a process log (alias of `expl process logs`).
    Logs(ProcessLogsArgs),
    /// Stop processes (alias of `expl process stop`).
    Stop(ProcessStopArgs),
    /// Kill processes (alias of `expl process kill`).
    Kill(ProcessTargetsArgs),
    /// Remove exited processes (alias of `expl process rm`).
    Rm(ProcessRmArgs),
}

/// `expl init`.
#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    /// Revision the origin node starts from.
    #[arg(long, default_value = "HEAD")]
    pub(crate) base: String,
    /// The trunk: the default branch expl follows, written to expl.toml
    /// (default: the branch checked out, or `--base` when it names one).
    #[arg(long, value_name = "BRANCH")]
    pub(crate) trunk: Option<String>,
}

/// `expl doctor`.
#[derive(Debug, Args)]
pub(crate) struct DoctorArgs {
    /// Repair what is safe: unclaim orphaned running steps so they are
    /// redelivered, and prune stale worktree registrations.
    #[arg(long)]
    pub(crate) fix: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ListingFormat::Table)]
    pub(crate) format: ListingFormat,
}

/// `expl origin ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum OriginCommand {
    /// List every origin, oldest first, with what was adopted into it.
    Ls(ListingArgs),
}

/// `expl goal ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum GoalCommand {
    /// Add a goal (records `goal.added`).
    Add(GoalAddArgs),
    /// List active goals (with --all, retired ones too).
    Ls(GoalLsArgs),
    /// Place a goal under other goals, as a subgoal of each, or with no
    /// --under back at the top level.
    Nest(GoalNestArgs),
    /// Retire a goal (records `goal.retired`).
    Retire(PruneArgs),
}

/// `expl goal nest`.
#[derive(Debug, Args)]
pub(crate) struct GoalNestArgs {
    /// The goal to place.
    pub(crate) goal: NodeId,
    /// The goals it is a subgoal of from now on, replacing the ones it had
    /// (repeatable; none makes it a top-level goal).
    #[arg(long, value_name = "GOAL")]
    pub(crate) under: Vec<NodeId>,
}

/// `expl goal add`.
#[derive(Debug, Args)]
pub(crate) struct GoalAddArgs {
    /// Short name.
    pub(crate) title: String,
    /// The goal statement (defaults to the title).
    #[arg(long)]
    pub(crate) objective: Option<String>,
    /// Also explore backward from this goal (counter-exploration).
    #[arg(long)]
    pub(crate) counter: bool,
    /// Make it a subgoal of this goal: a hypothesis about a route toward it
    /// (repeatable).
    #[arg(long, value_name = "GOAL")]
    pub(crate) under: Vec<NodeId>,
}

/// `expl goal ls`.
#[derive(Debug, Args)]
pub(crate) struct GoalLsArgs {
    /// Include retired goals.
    #[arg(short, long)]
    pub(crate) all: bool,
    /// Output options.
    #[command(flatten)]
    pub(crate) listing: ListingArgs,
}

/// `expl node ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum NodeCommand {
    /// List nodes.
    Ls(NodeLsArgs),
    /// Show one node with its edges, evaluations, and agent runs.
    Inspect(NodeRefArgs),
    /// Propose a step by hand (records `node.proposed`).
    Add(NodeAddArgs),
    /// Re-plan a failed step.
    Retry(NodeRefArgs),
    /// Prune a proposed, planned, or failed step.
    Prune(PruneArgs),
    /// Print the newest agent log about a node.
    Logs(NodeRefArgs),
}

/// `expl node ls`.
#[derive(Debug, Args)]
pub(crate) struct NodeLsArgs {
    /// Only these kinds.
    #[arg(long, value_parser = parse_kind)]
    pub(crate) kind: Vec<NodeKind>,
    /// Only these statuses.
    #[arg(long, value_parser = parse_status)]
    pub(crate) status: Vec<NodeStatus>,
    /// Output options.
    #[command(flatten)]
    pub(crate) listing: ListingArgs,
}

/// `expl node add`.
#[derive(Debug, Args)]
pub(crate) struct NodeAddArgs {
    /// Short name.
    pub(crate) title: String,
    /// What the step must achieve (defaults to the title).
    #[arg(long)]
    pub(crate) objective: Option<String>,
    /// Nodes this step builds on (default: the origin).
    #[arg(long, value_parser = parse_node)]
    pub(crate) after: Vec<NodeId>,
    /// Nodes this step leads toward.
    #[arg(long, value_parser = parse_node)]
    pub(crate) before: Vec<NodeId>,
    /// Plan it immediately at this priority, without waiting for planning.
    #[arg(long)]
    pub(crate) priority: Option<i64>,
    /// An investigation: a question to answer (the objective), changing
    /// nothing, rather than a change committed on its own branch.
    #[arg(long)]
    pub(crate) investigation: bool,
    /// The failed step this one is a new hypothesis grown from (recorded
    /// on the node; not an edge, since the failed step is not built on).
    #[arg(long, value_parser = parse_node)]
    pub(crate) derived_from: Option<NodeId>,
}

/// A command targeting one node.
#[derive(Debug, Args)]
pub(crate) struct NodeRefArgs {
    /// Node id (`7` or `#7`).
    #[arg(value_parser = parse_node)]
    pub(crate) node: NodeId,
}

/// `prune` / `retire`.
#[derive(Debug, Args)]
pub(crate) struct PruneArgs {
    /// Node id (`7` or `#7`).
    #[arg(value_parser = parse_node)]
    pub(crate) node: NodeId,
    /// Why.
    #[arg(long, default_value = "by operator")]
    pub(crate) reason: String,
}

/// `expl graph ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum GraphCommand {
    /// Render the graph.
    Show(GraphShowArgs),
}

/// `expl graph show`.
#[derive(Debug, Args)]
pub(crate) struct GraphShowArgs {
    /// Rendering.
    #[arg(long, value_enum, default_value_t = GraphFormat::Text)]
    pub(crate) format: GraphFormat,
}

/// Graph renderings.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum GraphFormat {
    /// Indented text for terminals.
    Text,
    /// Mermaid flowchart.
    Mermaid,
    /// One JSON object with nodes and edges.
    Json,
}

/// `expl path ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum PathCommand {
    /// List the latest plan's paths.
    Ls(ListingArgs),
}

/// `expl hypothesis ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum HypothesisCommand {
    /// List hypotheses that satisfy a goal, best first.
    Ls(ListingArgs),
}

/// `expl event ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum EventCommand {
    /// List events, oldest first.
    Ls(EventLsArgs),
}

/// `expl event ls`.
#[derive(Debug, Args)]
pub(crate) struct EventLsArgs {
    /// Only events after this id (default: the newest `--limit`).
    #[arg(long)]
    pub(crate) after: Option<i64>,
    /// At most this many events.
    #[arg(long, default_value_t = 50)]
    pub(crate) limit: usize,
    /// Only these kinds.
    #[arg(long, value_parser = parse_event_kind)]
    pub(crate) kind: Vec<EventKind>,
    /// Show how this agent's deliveries of each event stand.
    #[arg(long)]
    pub(crate) agent: Option<String>,
    /// Output options.
    #[command(flatten)]
    pub(crate) listing: ListingArgs,
}

/// `expl agent ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum AgentCommand {
    /// List declared agents with their running processes and backlog.
    Ls(ListingArgs),
}

/// `expl process ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum ProcessCommand {
    /// List processes.
    Ls(PsArgs),
    /// Show one process record.
    Inspect(ProcessTargetArgs),
    /// Print a process log.
    Logs(ProcessLogsArgs),
    /// Ask processes to stop, then kill them after a grace period.
    Stop(ProcessStopArgs),
    /// Kill processes immediately.
    Kill(ProcessTargetsArgs),
    /// Remove records (and logs) of exited processes.
    Rm(ProcessRmArgs),
}

/// `expl run`.
#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Declared agent name.
    pub(crate) agent: String,
    /// Run in the background and print the process name.
    #[arg(short, long)]
    pub(crate) detach: bool,
    /// Process name (default: `<agent>-<id>`).
    #[arg(long)]
    pub(crate) name: Option<String>,
    /// Exit after reacting to one event.
    #[arg(long)]
    pub(crate) once: bool,
    /// Exit when no event is deliverable.
    #[arg(long)]
    pub(crate) exit_when_idle: bool,
    /// Adopt a pre-created process record (used by `-d`).
    #[arg(long, hide = true, conflicts_with_all = ["detach", "name"])]
    pub(crate) adopt: Option<i64>,
}

/// `expl up`.
#[derive(Debug, Args)]
pub(crate) struct UpArgs {
    /// Only these agents (default: all declared).
    pub(crate) agents: Vec<String>,
}

/// `expl ps` / `expl process ls`.
#[derive(Debug, Args)]
pub(crate) struct PsArgs {
    /// Include exited and dead processes.
    #[arg(short, long)]
    pub(crate) all: bool,
    /// Output options.
    #[command(flatten)]
    pub(crate) listing: ListingArgs,
}

/// One process target.
#[derive(Debug, Args)]
pub(crate) struct ProcessTargetArgs {
    /// Process id or name.
    pub(crate) process: String,
}

/// Several process targets.
#[derive(Debug, Args)]
pub(crate) struct ProcessTargetsArgs {
    /// Process ids or names.
    #[arg(required = true)]
    pub(crate) processes: Vec<String>,
}

/// `logs`.
#[derive(Debug, Args)]
pub(crate) struct ProcessLogsArgs {
    /// Process id or name.
    pub(crate) process: String,
    /// Keep printing as the log grows, until the process exits.
    #[arg(short, long)]
    pub(crate) follow: bool,
}

/// Grace period for `stop` / `down`.
#[derive(Debug, Args)]
pub(crate) struct StopArgs {
    /// Seconds to wait before killing.
    #[arg(short, long, default_value_t = 30)]
    pub(crate) time: u64,
}

/// `stop`.
#[derive(Debug, Args)]
pub(crate) struct ProcessStopArgs {
    /// Process ids or names.
    #[arg(required = true)]
    pub(crate) processes: Vec<String>,
    /// Grace period.
    #[command(flatten)]
    pub(crate) stop: StopArgs,
}

/// `rm`.
#[derive(Debug, Args)]
pub(crate) struct ProcessRmArgs {
    /// Process ids or names.
    #[arg(required = true)]
    pub(crate) processes: Vec<String>,
    /// Kill live processes first.
    #[arg(short, long)]
    pub(crate) force: bool,
}

/// Shared listing options (iter's vocabulary).
#[derive(Debug, Args)]
pub(crate) struct ListingArgs {
    /// Print one id per line.
    #[arg(short, long)]
    pub(crate) quiet: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ListingFormat::Table)]
    pub(crate) format: ListingFormat,
}

/// Listing formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ListingFormat {
    /// Human table.
    Table,
    /// NDJSON, one object per line.
    Json,
}

fn parse_node(value: &str) -> Result<NodeId, String> {
    value
        .parse()
        .map_err(|_| format!("`{value}` is not a node id (expected e.g. 7 or #7)"))
}

fn parse_kind(value: &str) -> Result<NodeKind, String> {
    value.parse()
}

fn parse_status(value: &str) -> Result<NodeStatus, String> {
    value.parse()
}

fn parse_event_kind(value: &str) -> Result<EventKind, String> {
    value.parse()
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        super::Cli::command().debug_assert();
    }
}
