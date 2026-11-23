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
//! Consumers:  expl consumer ls
//! Processes:  expl run <CONSUMER> [-d]
//!             expl up [CONSUMER...] · expl down
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
    about = "Explore a problem space through graph events",
    long_about = "expl explores a problem space as a graph of hypotheses. Every change to the \
                  graph produces an event. Consumers in expl.toml subscribe to events and \
                  define the work to run when they arrive. Exploration proposes hypotheses, \
                  planning orders them, verification tests them with commits or findings, \
                  and evaluation judges the results against goals. Workers of the same \
                  consumer share deliveries, so independent work can run in parallel.",
    after_help = "EXAMPLES:\n  \
                  expl init\n  \
                  expl goal add \"Support Codex\" --objective \"...\"\n  \
                  expl up                       # every consumer, `replicas` workers each\n  \
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

    /// Shared daemon directory. A managed checkout uses its saved directory by default.
    #[arg(long, global = true, env = "EXPL_CLUSTER", value_name = "DIR")]
    pub(crate) cluster: Option<PathBuf>,

    /// Daemon member to run or contact.
    #[arg(long, global = true, env = "EXPL_MEMBER", default_value = "local")]
    pub(crate) member: String,

    /// The explorer an operation is about, when the daemon hosts several
    /// and the command is not run inside one of them.
    #[arg(long, global = true, env = "EXPL_EXPLORER", value_name = "NAME")]
    pub(crate) explorer: Option<String>,

    /// Subcommand to dispatch.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Serve as leader or member. A single daemon elects itself.
    #[command(
        long_about = "Serve the daemon directory as leader or member. A single daemon elects \
                      itself. Inside a project the project joins the daemon (its own \
                      .expl/daemon unless --cluster names another). With --cluster outside a \
                      project the daemon hosts whatever explorers are registered with it."
    )]
    Daemon,
    /// Join the project to the daemon named by --cluster, without starting one.
    Join,
    /// Show the selected daemon, the elected leader and the explorers it hosts.
    Cluster(ClusterArgs),
    /// Manage the explorers a daemon hosts.
    #[command(
        subcommand,
        long_about = "Manage the explorers registered with a daemon. An explorer is whatever \
                      registers consumers: which events they react to, the command executed \
                      for one event, and where it runs. A project made with `expl init` joins \
                      by `expl daemon` or `expl join`; any other explorer is added from a \
                      file. These commands need --cluster (or a project that joined)."
    )]
    Explorer(ExplorerCommand),
    #[command(hide = true)]
    RuntimeOverview,
    #[command(hide = true)]
    RuntimeJob { name: String, peer: String },
    /// One worker of a registered consumer. What the daemon starts.
    #[command(hide = true)]
    RuntimeWorker(RuntimeWorkerArgs),
    /// Put right what a lost leader left half done. What the daemon runs
    /// for this project after an election.
    #[command(hide = true)]
    RuntimeRecover,
    /// React, as a consumer, to the event a worker leased (described in
    /// the environment). What a worker executes for one event.
    #[command(hide = true)]
    React { consumer: String },
    /// Look once whether the trunk moved, and let the origin follow it.
    /// The periodic task of every worker.
    #[command(hide = true)]
    Follow,
    /// Revisit a random starting point. What an idle exploration runs.
    #[command(hide = true)]
    Revisit { consumer: String },
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
    /// Show event consumers.
    #[command(subcommand, alias = "agent")]
    Consumer(ConsumerCommand),
    /// Manage processes.
    #[command(subcommand)]
    Process(ProcessCommand),
    /// Start a worker for an event consumer.
    #[command(
        long_about = "Start a worker for a declared consumer. It leases subscribed events one \
                      at a time, runs the handler, and acknowledges the delivery. Replicas \
                      share the same inbox. With -d the process runs in the background and \
                      prints its name."
    )]
    Run(RunArgs),
    /// Start workers until each assigned consumer has `replicas` running.
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

#[derive(Debug, Args)]
pub(crate) struct ClusterArgs {
    #[command(subcommand)]
    pub(crate) command: ClusterCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ClusterCommand {
    /// Show the daemon role and the current leader as JSON.
    Status,
}

/// The worker the daemon starts for a registered consumer.
#[derive(Debug, Args)]
pub(crate) struct RuntimeWorkerArgs {
    /// The registered explorer.
    #[arg(long)]
    pub(crate) explorer: String,
    /// One of its consumers.
    #[arg(long)]
    pub(crate) consumer: String,
    /// The process record to adopt.
    #[arg(long)]
    pub(crate) adopt: i64,
    /// Exit after reacting to one event.
    #[arg(long)]
    pub(crate) once: bool,
    /// Exit when there is no work to deliver.
    #[arg(long)]
    pub(crate) exit_when_idle: bool,
}

/// `expl explorer ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum ExplorerCommand {
    /// List the registered explorers.
    Ls(ListingArgs),
    /// Register an explorer from a TOML (or `*.json`) file, creating the
    /// framework's tables in its database when it has none.
    Add(ExplorerAddArgs),
    /// Remove an explorer from the daemon (its workers must be stopped).
    Rm(ExplorerNameArgs),
    /// Start workers until each of its consumers has `replicas` running.
    Up(ExplorerUpArgs),
    /// Stop every worker of an explorer.
    Down(ExplorerDownArgs),
    /// List an explorer's workers.
    Ps(ExplorerPsArgs),
    /// Append an event to an explorer's event log.
    Publish(ExplorerPublishArgs),
}

/// `expl explorer add`.
#[derive(Debug, Args)]
pub(crate) struct ExplorerAddArgs {
    /// The explorer's file: name, database, and its consumers, tasks and
    /// routing. Paths in it are relative to the file.
    pub(crate) file: PathBuf,
}

/// One registered explorer.
#[derive(Debug, Args)]
pub(crate) struct ExplorerNameArgs {
    /// The explorer's registered name.
    pub(crate) name: String,
}

/// `expl explorer up`.
#[derive(Debug, Args)]
pub(crate) struct ExplorerUpArgs {
    /// The explorer's registered name.
    pub(crate) name: String,
    /// Only these consumers (default: all it registers).
    pub(crate) consumers: Vec<String>,
}

/// `expl explorer down`.
#[derive(Debug, Args)]
pub(crate) struct ExplorerDownArgs {
    /// The explorer's registered name.
    pub(crate) name: String,
    /// Grace period.
    #[command(flatten)]
    pub(crate) stop: StopArgs,
}

/// `expl explorer ps`.
#[derive(Debug, Args)]
pub(crate) struct ExplorerPsArgs {
    /// The explorer's registered name.
    pub(crate) name: String,
    /// Which workers, and how to print them.
    #[command(flatten)]
    pub(crate) ps: PsArgs,
}

/// `expl explorer publish`.
#[derive(Debug, Args)]
pub(crate) struct ExplorerPublishArgs {
    /// The explorer's registered name.
    pub(crate) name: String,
    /// The event's kind, in the explorer's own vocabulary.
    pub(crate) kind: String,
    /// What the event is about, as the explorer numbers it.
    #[arg(long, value_name = "N")]
    pub(crate) subject: Option<i64>,
    /// Data carried with the event, as JSON.
    #[arg(long, value_name = "JSON")]
    pub(crate) payload: Option<String>,
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
    /// Show delivery status for this consumer.
    #[arg(long, alias = "agent")]
    pub(crate) consumer: Option<String>,
    /// Output options.
    #[command(flatten)]
    pub(crate) listing: ListingArgs,
}

/// `expl consumer ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum ConsumerCommand {
    /// List event consumers with their subscriptions, running workers, and backlog.
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
    /// Declared consumer name.
    pub(crate) consumer: String,
    /// Run in the background and print the process name.
    #[arg(short, long)]
    pub(crate) detach: bool,
    /// Process name (default: `<consumer>-<id>`).
    #[arg(long)]
    pub(crate) name: Option<String>,
    /// Exit after reacting to one event.
    #[arg(long)]
    pub(crate) once: bool,
    /// Exit when there is no work to deliver. Wait for shared capacity when needed.
    #[arg(long)]
    pub(crate) exit_when_idle: bool,
    /// Adopt a pre-created process record (used by `-d`).
    #[arg(long, hide = true, conflicts_with_all = ["detach", "name"])]
    pub(crate) adopt: Option<i64>,
}

/// `expl up`.
#[derive(Debug, Args)]
pub(crate) struct UpArgs {
    /// Only these consumers (default: all declared).
    pub(crate) consumers: Vec<String>,
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
