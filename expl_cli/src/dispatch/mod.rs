//! Command dispatch: one module per resource or verb.

mod agent;
mod candidate;
mod event;
mod goal;
mod graph;
mod init;
mod logs;
mod node;
mod origin;
mod path;
mod process;
mod run;
mod up;

use std::path::Path;

use expl_core::{Context, Declaration, Graph, Processes, Project};

use crate::cli::{
    AgentCommand, CandidateCommand, Cli, Command, EventCommand, GraphCommand, OriginCommand,
    PathCommand, ProcessCommand,
};
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
        Command::Graph(GraphCommand::Show(args)) => graph::show(&start, &args),
        Command::Path(PathCommand::Ls(args)) => path::ls(&start, &args),
        Command::Candidate(CandidateCommand::Ls(args)) => candidate::ls(&start, &args),
        Command::Origin(OriginCommand::Ls(args)) => origin::ls(&start, &args),
        Command::Event(EventCommand::Ls(args)) => event::ls(&start, &args),
        Command::Agent(AgentCommand::Ls(args)) => agent::ls(&start, &args),
        Command::Process(command) => process::process(&start, command),
        Command::Run(args) => run::run(&start, &args),
        Command::Up(args) => up::up(&start, &args),
        Command::Down(args) => up::down(&start, &args),
        Command::Ps(args) => process::process(&start, ProcessCommand::Ls(args)),
        Command::Logs(args) => process::process(&start, ProcessCommand::Logs(args)),
        Command::Stop(args) => process::process(&start, ProcessCommand::Stop(args)),
        Command::Kill(args) => process::process(&start, ProcessCommand::Kill(args)),
        Command::Rm(args) => process::process(&start, ProcessCommand::Rm(args)),
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

/// The process table of `project`.
pub(crate) fn processes(project: &Project) -> Result<Processes, CliError> {
    Ok(Processes::open(
        &project.database_path(),
        &project.processes_dir(),
    )?)
}
