//! Bind this binary to the runtime's daemon.
//!
//! The daemon is the framework's and hosts any explorer registered with it.
//! This binary is its executable: `expl daemon` serves, the hidden
//! `runtime-job` and `runtime-worker` commands are what a daemon starts,
//! and every other command typed against a daemon is forwarded to the
//! leader. What is specific to expl is only how a project joins: its
//! `expl.toml` becomes the registration.

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use expl_core::{Declaration, Interrupt, ProcessId, ProcessOptions, Project};
use expl_runtime::cluster::{Authority, Cluster, Peer};
use expl_runtime::control::Control;
use expl_runtime::daemon::{self, Explorer, Registry};

use crate::cli::{self, Cli, RuntimeWorkerArgs};
use crate::dispatch;
use crate::error::CliError;
use crate::output::{json_pretty, status};
use crate::telemetry::Telemetry;

pub(crate) fn dispatch(mut cli: Cli) -> Result<u8, CliError> {
    if let cli::Command::RuntimeJob { name, peer } = &cli.command {
        let directory = cli
            .cluster
            .as_deref()
            .ok_or_else(|| CliError::Usage("missing runtime directory".to_owned()))?;
        daemon::run_job(directory, name, peer).map_err(cluster_error)?;
        return Ok(0);
    }
    if let cli::Command::RuntimeWorker(args) = &cli.command {
        worker(cli.cluster.as_deref(), args)?;
        return Ok(0);
    }
    // Started by the elected daemon: this is the leader's side of an
    // operation, or something a worker executes.
    if let Some((authority, _term)) = daemon::enter().map_err(cluster_error)? {
        if let cli::Command::Explorer(command) = cli.command {
            dispatch::explorer::explorer(&authority, command)?;
            return Ok(0);
        }
        let directory = daemon::bound_directory()
            .ok_or_else(|| CliError::Usage("missing daemon project".to_owned()))?;
        cli.directory = Some(directory);
        dispatch::dispatch(cli)?;
        return Ok(0);
    }
    let start = cli
        .directory
        .clone()
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|error| CliError::io("current directory", error))?;
    if matches!(cli.command, cli::Command::Daemon) {
        serve(&start, cli.cluster.as_deref(), &cli.member)?;
        return Ok(0);
    }
    if matches!(cli.command, cli::Command::Join) {
        let project = Project::discover(&start)?;
        let cluster = join_project(&project, cli.cluster.as_deref())?;
        status(&format!(
            "{} joined the daemon at {}",
            project.explorer_name(),
            cluster.root().display()
        ));
        return Ok(0);
    }
    let Some(cluster) = locate(&start, cli.cluster.as_deref())? else {
        if matches!(cli.command, cli::Command::Cluster(_)) {
            return Err(CliError::Usage(
                "no daemon configured for this project".to_owned(),
            ));
        }
        if matches!(cli.command, cli::Command::Explorer(_)) {
            return Err(CliError::Usage(
                "explorer commands need a daemon: name its directory with --cluster".to_owned(),
            ));
        }
        dispatch::dispatch(cli)?;
        return Ok(0);
    };
    let via = cluster.member(&cli.member).map_err(cluster_error)?;
    if let cli::Command::Cluster(args) = &cli.command {
        return match args.command {
            cli::ClusterCommand::Status => {
                cluster_status(&cluster, &cli.member, via.as_ref())?;
                Ok(0)
            }
        };
    }
    let via = via.ok_or_else(|| CliError::Usage(format!("daemon `{}` is offline", cli.member)))?;
    let directory = if matches!(cli.command, cli::Command::Explorer(_)) {
        // About the daemon itself, not one explorer.
        cluster.root().to_path_buf()
    } else {
        target(&cluster, &start, cli.explorer.as_deref())?
    };
    let interrupt = Interrupt::on_signals().map_err(cluster_error)?;
    daemon::forward(
        &cluster,
        &via,
        std::env::args_os().skip(1),
        &directory,
        &interrupt,
    )
    .map_err(cluster_error)
}

fn cluster_error(error: io::Error) -> CliError {
    CliError::io("daemon", error)
}

fn cluster_status(cluster: &Cluster, member: &str, via: Option<&Peer>) -> Result<(), CliError> {
    let leader = cluster.leader().map_err(cluster_error)?;
    let role = match via {
        Some(member) if Some(member) == leader.as_ref() => "leader",
        Some(_) => "member",
        None => "offline",
    };
    let explorers: Vec<String> = Registry::of(cluster)
        .explorers()?
        .into_iter()
        .map(|explorer| explorer.name)
        .collect();
    // The limits the operator set, and how much of each is taken now.
    let control = Control::load(cluster)?;
    let agents = serde_json::json!({
        "concurrency": control.agents.concurrency,
        "in_use": control.usage(cluster).map_err(cluster_error)?.map(|usage| usage.in_use),
        "named": control.agent.keys().collect::<Vec<_>>(),
    });
    json_pretty(&serde_json::json!({
        "member": member,
        "role": role,
        "leader": leader.as_ref().map(Peer::name),
        "explorers": explorers,
        "agents": agents,
        "pools": control.pool_usage(cluster).map_err(cluster_error)?,
    }))
}

/// The directory of the explorer a forwarded operation is about: the one
/// named, else the project the command was typed in, else the daemon's
/// only explorer.
fn target(cluster: &Cluster, start: &Path, named: Option<&str>) -> Result<PathBuf, CliError> {
    let registry = Registry::of(cluster);
    if let Some(name) = named {
        return Ok(registry.expect(name)?.directory);
    }
    if let Ok(project) = Project::discover(start) {
        return Ok(project.root().to_path_buf());
    }
    let mut explorers = registry.explorers()?;
    match explorers.len() {
        0 => {
            // A daemon directory from before explorers registered.
            legacy_project(cluster).ok_or_else(|| {
                CliError::Usage("no explorer is registered with this daemon".to_owned())
            })
        }
        1 => Ok(explorers.remove(0).directory),
        _ => Err(CliError::Usage(format!(
            "this daemon hosts several explorers ({}): name one with --explorer, or run the \
             command inside it",
            explorers
                .iter()
                .map(|explorer| explorer.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The one project a daemon directory from before the registry belonged to.
fn legacy_project(cluster: &Cluster) -> Option<PathBuf> {
    fs::read_to_string(cluster.root().join("project"))
        .ok()
        .map(PathBuf::from)
}

fn locate(start: &Path, directory: Option<&Path>) -> Result<Option<Cluster>, CliError> {
    if let Some(directory) = directory {
        return Cluster::open(directory).map(Some).map_err(cluster_error);
    }
    let Ok(project) = Project::for_repository(start) else {
        // The regular dispatcher owns repository and init diagnostics.
        return Ok(None);
    };
    configured(&project)?
        .as_deref()
        .map(Cluster::open)
        .transpose()
        .map_err(cluster_error)
}

fn configured(project: &Project) -> Result<Option<PathBuf>, CliError> {
    match fs::read_to_string(project.cluster_path()) {
        Ok(path) => Ok(Some(PathBuf::from(path))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(cluster_error(error)),
    }
}

/// What a project registers with a daemon: its `expl.toml`, with each
/// consumer's reaction as a command of this binary.
fn record(project: &Project, declaration: &Declaration) -> Result<Explorer, CliError> {
    let exe = std::env::current_exe().map_err(|e| CliError::io("locate the expl binary", e))?;
    let program = vec![
        exe.to_string_lossy().into_owned(),
        "-C".to_owned(),
        project.root().to_string_lossy().into_owned(),
    ];
    let mut recover = program.clone();
    recover.push("runtime-recover".to_owned());
    Ok(Explorer {
        name: project.explorer_name(),
        directory: project.root().to_path_buf(),
        database: project.database_path(),
        state: project.state_dir(),
        logs: project.processes_dir(),
        program,
        recover,
        telemetry: declaration
            .telemetry
            .as_ref()
            .map(|telemetry| telemetry.endpoint.trim().to_owned()),
        registration: declaration.registration_for(&exe, project.root()),
    })
}

/// Register `project` with `cluster` as its `expl.toml` stands now, and
/// return what was registered.
pub(crate) fn register(project: &Project, cluster: &Cluster) -> Result<Explorer, CliError> {
    let declaration = Declaration::load(&project.declaration_path())?;
    let explorer = record(project, &declaration)?;
    // The runtime opens the database as it is: bring it up to date first.
    dispatch::processes(project)?;
    Registry::of(cluster).join(&explorer)?;
    Ok(explorer)
}

/// Make `project` an explorer of the daemon in `directory` (the daemon it
/// already uses, or its own `.expl/daemon`, when none is named).
fn join_project(project: &Project, directory: Option<&Path>) -> Result<Cluster, CliError> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(project.state_dir().join("cluster-config.lock"))
        .map_err(cluster_error)?;
    lock.lock().map_err(cluster_error)?;
    let configured = configured(project)?;
    let root = directory
        .map(Path::to_path_buf)
        .or_else(|| configured.clone())
        .unwrap_or_else(|| project.state_dir().join("daemon"));
    let cluster = Cluster::open(&root).map_err(cluster_error)?;
    if let Some(existing) = configured {
        if existing != cluster.root() {
            return Err(CliError::Usage(format!(
                "project already uses daemon directory {}",
                existing.display()
            )));
        }
    } else {
        if !dispatch::processes(project)?.list(false)?.is_empty() {
            return Err(CliError::Usage(
                "stop local workers with `expl down` before starting the first daemon".to_owned(),
            ));
        }
        daemon::remember(&project.cluster_path(), cluster.root()).map_err(cluster_error)?;
    }
    register(project, &cluster)?;
    // Before explorers registered, a daemon directory named its one project.
    if legacy_project(&cluster).as_deref() == Some(project.root()) {
        fs::remove_file(cluster.root().join("project")).ok();
    }
    Ok(cluster)
}

fn serve(start: &Path, directory: Option<&Path>, member: &str) -> Result<(), CliError> {
    let cluster = match (Project::discover(start), directory) {
        // Inside a project: it joins the daemon it starts.
        (Ok(project), directory) => join_project(&project, directory)?,
        // A daemon of its own: it hosts whatever registers.
        (Err(_), Some(directory)) => Cluster::open(directory).map_err(cluster_error)?,
        (Err(error), None) => return Err(error.into()),
    };
    let exe = std::env::current_exe().map_err(cluster_error)?;
    let interrupt = Interrupt::on_signals().map_err(cluster_error)?;
    daemon::serve(&cluster, member, &exe, &interrupt, &|note| status(note))?;
    Ok(())
}

/// Be one worker of a registered consumer.
fn worker(directory: Option<&Path>, args: &RuntimeWorkerArgs) -> Result<(), CliError> {
    let directory =
        directory.ok_or_else(|| CliError::Usage("missing runtime directory".to_owned()))?;
    let cluster = Cluster::open(directory).map_err(cluster_error)?;
    let explorer = Registry::of(&cluster).expect(&args.explorer)?;
    let interrupt =
        Interrupt::on_signals().map_err(|e| CliError::io("install signal handlers", e))?;
    let _telemetry = Telemetry::start_named(&explorer.name, explorer.telemetry.as_deref(), true);
    daemon::run_worker(
        &explorer,
        &args.consumer,
        ProcessId(args.adopt),
        ProcessOptions {
            once: args.once,
            exit_when_idle: args.exit_when_idle,
        },
        interrupt,
    )?;
    Ok(())
}

/// The elected daemon this command runs under, if one started it.
pub(crate) fn authority() -> Result<Option<Authority>, CliError> {
    Ok(daemon::enter()
        .map_err(cluster_error)?
        .map(|(authority, _)| authority))
}
