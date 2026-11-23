//! Command dispatch: one module per resource or verb.

mod consumer;
mod doctor;
mod event;
pub(crate) mod explorer;
mod goal;
mod graph;
mod hypothesis;
mod init;
mod logs;
mod node;
mod origin;
mod path;
mod process;
mod react;
pub(crate) mod run;
mod up;

use std::path::Path;

use expl_core::{Context, Declaration, Graph, Processes, Project};

use crate::cli::{
    Cli, Command, ConsumerCommand, EventCommand, GraphCommand, HypothesisCommand, OriginCommand,
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
        Command::Daemon
        | Command::Join
        | Command::Cluster(_)
        | Command::Explorer(_)
        | Command::RuntimeJob { .. }
        | Command::RuntimeWorker(_) => Err(CliError::Usage(
            "daemon command must run at the client entry point".to_owned(),
        )),
        Command::RuntimeRecover => {
            // The workers of the last term are gone, and with them the
            // agent runs they journaled as running.
            let (_, mut graph) = open(&start)?;
            Ok(graph.interrupt_agent_runs("leader changed")?)
        }

        Command::RuntimeOverview => {
            let project = Project::discover(&start)?;
            let view = expl_core::overview::Overview::load(&project).map_err(CliError::Overview)?;
            crate::output::json_pretty(&view)
        }
        Command::React { consumer } => react::react(&start, &consumer),
        Command::Follow => react::follow(&start),
        Command::Revisit { consumer } => react::revisit(&start, &consumer),
        Command::Init(args) => init::init(&start, &args),
        Command::Doctor(args) => doctor::doctor(&start, &args),
        Command::Goal(command) => goal::goal(&start, command),
        Command::Node(command) => node::node(&start, command),
        Command::Graph(GraphCommand::Show(args)) => graph::show(&start, &args),
        Command::Path(PathCommand::Ls(args)) => path::ls(&start, &args),
        Command::Hypothesis(HypothesisCommand::Ls(args)) => hypothesis::ls(&start, &args),
        Command::Origin(OriginCommand::Ls(args)) => origin::ls(&start, &args),
        Command::Event(EventCommand::Ls(args)) => event::ls(&start, &args),
        Command::Consumer(ConsumerCommand::Ls(args)) => consumer::ls(&start, &args),
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
    let context = Context::new(project, declaration);
    Ok(match crate::daemon::authority()? {
        Some(authority) => context.with_authority(authority),
        None => context,
    })
}

/// The process table of `project`.
pub(crate) fn processes(project: &Project) -> Result<Processes, CliError> {
    Ok(expl_core::process::processes(project)?)
}
