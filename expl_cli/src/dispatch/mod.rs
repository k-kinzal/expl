//! Command dispatch: one module per resource or verb.

mod candidate;
mod goal;
mod graph;
mod init;
mod logs;
mod node;
mod path;
mod role;
mod run;

use std::path::Path;

use expl_core::{Context, Declaration, Graph, Project};

use crate::cli::{CandidateCommand, Cli, Command, GraphCommand, PathCommand};
use crate::error::CliError;

/// Run the parsed command.
pub(crate) fn dispatch(cli: Cli) -> Result<(), CliError> {
    let start = match cli.directory {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| CliError::io("current directory", e))?,
    };
    match cli.command {
        Command::Init(args) => init::init(&start, &args),
        Command::Goal(command) => goal::goal(&start, command),
        Command::Node(command) => node::node(&start, command),
        Command::Inspect(args) => node::inspect(&start, &args),
        Command::Graph(GraphCommand::Show(args)) => graph::show(&start, &args),
        Command::Path(PathCommand::Ls(args)) => path::ls(&start, &args),
        Command::Candidate(CandidateCommand::Ls(args)) => candidate::ls(&start, &args),
        Command::Logs(args) => logs::logs(&start, &args),
        Command::Explore(args) => role::explore(&start, &args),
        Command::Plan => role::plan(&start),
        Command::Execute(args) => role::execute(&start, &args),
        Command::Evaluate(args) => role::evaluate(&start, &args),
        Command::Run(args) => run::run(&start, &args),
    }
}

/// The initialised project around `start` and its graph.
pub(crate) fn open(start: &Path) -> Result<(Project, Graph), CliError> {
    let project = Project::discover(start)?;
    let graph = Graph::open(&project.database_path())?;
    Ok((project, graph))
}

/// The project around `start` bound to its declaration.
pub(crate) fn load(start: &Path) -> Result<Context, CliError> {
    let project = Project::discover(start)?;
    let declaration = Declaration::load(&project.declaration_path())?;
    Ok(Context::new(project, declaration))
}
