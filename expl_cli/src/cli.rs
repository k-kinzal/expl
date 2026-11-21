//! `clap` derive structures for the `expl` CLI.
//!
//! Resources follow iter's `<resource> <verb>` form; the four roles and the
//! runner are top-level verbs because they act on the whole graph:
//!
//! ```text
//! Setup:      expl init
//! Resources:  expl goal add|ls|prune
//!             expl node ls|inspect|add|retry|prune   (alias: expl inspect)
//!             expl graph show
//!             expl path ls
//!             expl candidate ls
//!             expl logs <NODE>
//! Roles:      expl explore | plan | execute | evaluate
//! Runner:     expl run
//! ```

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use expl_core::{Direction, NodeId, NodeKind, NodeStatus};

/// Top-level `expl` CLI definition.
#[derive(Debug, Parser)]
#[command(
    name = "expl",
    version,
    about = "Explore a problem space as a graph of agent-realised git branches",
    long_about = "expl explores a problem space as a graph. An explorer proposes nodes toward \
                  goals (and, per goal, backward from them), a planner draws paths and sets \
                  priorities, executors realise nodes as commits on their own git branches in \
                  parallel worktrees, and an evaluator judges them against the goals. The graph \
                  lives in SQLite under .expl/. Exploration never ends by itself: choosing among \
                  the candidates that satisfy a goal is yours.",
    after_help = "EXAMPLES:\n  \
                  expl init\n  \
                  expl goal add \"Support Codex as executor\" --objective \"...\"\n  \
                  expl run --cycles 3\n  \
                  expl graph show --format mermaid\n  \
                  expl candidate ls\n  \
                  git merge expl/node-7"
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
        long_about = "Create .expl/ (graph database, worktrees, logs), write expl.toml if \
                            absent, exclude .expl/ from git, and record the origin node at the \
                            base commit."
    )]
    Init(InitArgs),
    /// Manage goals.
    #[command(subcommand)]
    Goal(GoalCommand),
    /// Inspect and adjust nodes.
    #[command(subcommand)]
    Node(NodeCommand),
    /// Show one node (alias of `expl node inspect`).
    Inspect(InspectArgs),
    /// Render the whole graph.
    #[command(subcommand)]
    Graph(GraphCommand),
    /// Show the latest plan's paths.
    #[command(subcommand)]
    Path(PathCommand),
    /// Show steps evaluated as satisfying a goal.
    #[command(subcommand)]
    Candidate(CandidateCommand),
    /// Print the newest agent log for a node.
    Logs(LogsArgs),
    /// Run the explorer once.
    Explore(ExploreArgs),
    /// Run the planner once.
    Plan,
    /// Run the executor on one ready step.
    Execute(ExecuteArgs),
    /// Run the evaluator on completed steps.
    Evaluate(EvaluateArgs),
    /// Repeat explore → plan → execute → evaluate cycles.
    #[command(
        long_about = "Repeat cycles of explore → plan → execute → evaluate. Runs until \
                            --cycles is reached, a cycle finds nothing to do, or interrupted. \
                            Steps left running by an interrupted run are re-planned on the next \
                            start."
    )]
    Run(RunArgs),
}

/// `expl init`.
#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    /// Revision the origin node starts from.
    #[arg(long, default_value = "HEAD")]
    pub(crate) base: String,
}

/// `expl goal ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum GoalCommand {
    /// Add a goal.
    Add(GoalAddArgs),
    /// List active goals.
    Ls(ListingArgs),
    /// Retire a goal.
    Prune(PruneArgs),
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
}

/// `expl node ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum NodeCommand {
    /// List nodes.
    Ls(NodeLsArgs),
    /// Show one node with its edges, evaluations, and agent runs.
    Inspect(InspectArgs),
    /// Propose a step by hand.
    Add(NodeAddArgs),
    /// Re-plan a failed step.
    Retry(NodeRefArgs),
    /// Prune a proposed or planned step.
    Prune(PruneArgs),
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
    /// Plan it immediately at this priority.
    #[arg(long)]
    pub(crate) priority: Option<i64>,
}

/// A command targeting one node.
#[derive(Debug, Args)]
pub(crate) struct NodeRefArgs {
    /// Node id (`7` or `#7`).
    #[arg(value_parser = parse_node)]
    pub(crate) node: NodeId,
}

/// `inspect`.
#[derive(Debug, Args)]
pub(crate) struct InspectArgs {
    /// Node id (`7` or `#7`).
    #[arg(value_parser = parse_node)]
    pub(crate) node: NodeId,
}

/// `prune`.
#[derive(Debug, Args)]
pub(crate) struct PruneArgs {
    /// Node id (`7` or `#7`).
    #[arg(value_parser = parse_node)]
    pub(crate) node: NodeId,
    /// Why.
    #[arg(long, default_value = "pruned by operator")]
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

/// `expl candidate ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum CandidateCommand {
    /// List candidates, best first.
    Ls(ListingArgs),
}

/// `expl logs`.
#[derive(Debug, Args)]
pub(crate) struct LogsArgs {
    /// Node id (`7` or `#7`).
    #[arg(value_parser = parse_node)]
    pub(crate) node: NodeId,
}

/// `expl explore`.
#[derive(Debug, Args)]
pub(crate) struct ExploreArgs {
    /// Node to explore from (default: the frontier, or the
    /// counter-exploration goal for `--direction backward`).
    #[arg(long, value_parser = parse_node)]
    pub(crate) from: Option<NodeId>,
    /// Search direction.
    #[arg(long, value_parser = parse_direction, default_value = "forward")]
    pub(crate) direction: Direction,
    /// Output options.
    #[command(flatten)]
    pub(crate) report: ReportArgs,
}

/// `expl execute`.
#[derive(Debug, Args)]
pub(crate) struct ExecuteArgs {
    /// Step to execute (default: the highest-priority ready step).
    #[arg(value_parser = parse_node)]
    pub(crate) node: Option<NodeId>,
    /// Output options.
    #[command(flatten)]
    pub(crate) report: ReportArgs,
}

/// `expl evaluate`.
#[derive(Debug, Args)]
pub(crate) struct EvaluateArgs {
    /// Step to evaluate (default: every completed step not yet evaluated).
    #[arg(value_parser = parse_node)]
    pub(crate) node: Option<NodeId>,
    /// Output options.
    #[command(flatten)]
    pub(crate) report: ReportArgs,
}

/// `expl run`.
#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Stop after this many cycles.
    #[arg(long)]
    pub(crate) cycles: Option<usize>,
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

/// Role report options.
#[derive(Debug, Args)]
pub(crate) struct ReportArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = ReportFormat::Text)]
    pub(crate) format: ReportFormat,
}

/// Role report formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ReportFormat {
    /// Human summary.
    Text,
    /// One JSON object.
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

fn parse_direction(value: &str) -> Result<Direction, String> {
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
