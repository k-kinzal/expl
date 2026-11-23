//! Bind the exploration application to the runtime's daemon and command queue.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};

use expl_core::{Interrupt, Project};
use expl_runtime::cluster::{Authority, Cluster, CommandSpec, Outcome, Peer};

use crate::cli::{self, Cli};
use crate::dispatch;
use crate::error::CliError;
use crate::output::json_pretty;

const SESSION: &str = "EXPL_RUNTIME_SESSION";
const PROJECT: &str = "EXPL_RUNTIME_PROJECT";
const CLUSTER: &str = "EXPL_RUNTIME_CLUSTER";

pub(crate) fn dispatch(mut cli: Cli) -> Result<u8, CliError> {
    if let cli::Command::RuntimeJob { name, peer } = cli.command {
        let directory = cli
            .cluster
            .ok_or_else(|| CliError::Usage("missing runtime directory".to_owned()))?;
        run_job(&directory, &name, &peer)?;
        return Ok(0);
    }
    if let Some((_, _guard)) = enter()? {
        let project = std::env::var_os(PROJECT)
            .ok_or_else(|| CliError::Usage("missing daemon project".to_owned()))?;
        cli.directory = Some(PathBuf::from(project));
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
    let Some(cluster) = locate(&start, cli.cluster.as_deref())? else {
        if matches!(cli.command, cli::Command::Cluster(_)) {
            return Err(CliError::Usage(
                "no daemon configured for this project".to_owned(),
            ));
        }
        dispatch::dispatch(cli)?;
        return Ok(0);
    };
    let via = cluster.member(&cli.member).map_err(cluster_error)?;
    if let cli::Command::Cluster(args) = cli.command {
        return match args.command {
            cli::ClusterCommand::Status => {
                let leader = cluster.leader().map_err(cluster_error)?;
                let role = match &via {
                    Some(member) if Some(member) == leader.as_ref() => "leader",
                    Some(_) => "member",
                    None => "offline",
                };
                json_pretty(&serde_json::json!({
                    "member": cli.member, "role": role, "leader": leader.as_ref().map(Peer::name),
                }))?;
                Ok(0)
            }
        };
    }
    let via = via.ok_or_else(|| CliError::Usage(format!("daemon `{}` is offline", cli.member)))?;
    let mut command = Command::new("expl");
    command.args(std::env::args_os().skip(1));
    let root = project_root(&cluster)?;
    let interrupt = Interrupt::on_signals().map_err(cluster_error)?;
    let result = cluster
        .control(
            &via,
            CommandSpec::capture(&command, &root),
            &interrupt,
            &mut io::stdout(),
            &mut io::stderr(),
        )
        .map_err(cluster_error)?;
    match result {
        Outcome::Exited(status) => {
            let status = ExitStatus::from_raw(status);
            Ok(status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(2))
        }
        Outcome::Interrupted if interrupt.is_raised() => Ok(130),
        Outcome::Interrupted => Err(cluster_error(io::Error::other(
            "leader interrupted the operation",
        ))),
        Outcome::Failed(reason) => Err(cluster_error(io::Error::other(reason))),
        Outcome::TimedOut | Outcome::Lingering { .. } => Err(cluster_error(io::Error::other(
            "daemon operation did not finish",
        ))),
    }
}

fn cluster_error(error: io::Error) -> CliError {
    CliError::io("daemon", error)
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

fn project_root(cluster: &Cluster) -> Result<PathBuf, CliError> {
    fs::read_to_string(cluster.root().join("project"))
        .map(PathBuf::from)
        .map_err(cluster_error)
}

fn configure(project: &Project, directory: Option<&Path>) -> Result<Cluster, CliError> {
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
        set_path(&cluster.root().join("project"), project.root())?;
        set_path(&project.cluster_path(), cluster.root())?;
    }
    if project_root(&cluster)? != project.root() {
        return Err(CliError::Usage(
            "daemon directory belongs to another checkout".to_owned(),
        ));
    }
    Ok(cluster)
}

fn set_path(file: &Path, value: &Path) -> Result<(), CliError> {
    let value = value
        .to_str()
        .ok_or_else(|| CliError::Usage("daemon paths must be UTF-8".to_owned()))?;
    let write = || -> io::Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(file.parent().expect("file parent"))?;
        temp.write_all(value.as_bytes())?;
        match temp.persist_noclobber(file) {
            Ok(_) => Ok(()),
            Err(error)
                if error.error.kind() == ErrorKind::AlreadyExists
                    && fs::read_to_string(file)? == value =>
            {
                Ok(())
            }
            Err(error) => Err(error.error),
        }
    };
    write().map_err(cluster_error)
}

fn serve(start: &Path, directory: Option<&Path>, member: &str) -> Result<(), CliError> {
    let project = Project::discover(start)?;
    let cluster = configure(&project, directory)?;
    let exe = std::env::current_exe().map_err(cluster_error)?;
    let interrupt = Interrupt::on_signals().map_err(cluster_error)?;
    let recovering = RefCell::new(Vec::new());
    let mut workers: Vec<Child> = Vec::new();
    cluster
        .serve(
            member,
            &interrupt,
            |_| {
                let mut table = dispatch::processes(&project).map_err(io::Error::other)?;
                *recovering.borrow_mut() = table.recover().map_err(io::Error::other)?;
                // The workers of the last term are gone, and with them the
                // agent runs they journaled as running.
                expl_core::Graph::open(&project.database_path())
                    .and_then(|mut graph| graph.interrupt_agent_runs("leader changed"))
                    .map_err(io::Error::other)?;
                Ok(())
            },
            |authority| {
                for (record, options) in recovering.take() {
                    let child =
                        dispatch::run::spawn_detached(&project, &record, options, Some(authority))
                            .map_err(io::Error::other)?;
                    workers.push(child);
                }
                let mut index = 0;
                while index < workers.len() {
                    if workers[index].try_wait()?.is_some() {
                        workers.swap_remove(index).wait()?;
                    } else {
                        index += 1;
                    }
                }
                Ok(())
            },
            |job, peer| {
                Command::new(&exe)
                    .arg("--cluster")
                    .arg(cluster.root())
                    .arg("runtime-job")
                    .arg(job)
                    .arg(serde_json::to_string(peer)?)
                    .current_dir(project.root())
                    .process_group(0)
                    .spawn()
            },
        )
        .map_err(cluster_error)
}

fn run_job(directory: &Path, name: &str, peer: &str) -> Result<(), CliError> {
    let cluster = Cluster::open(directory).map_err(cluster_error)?;
    let peer = serde_json::from_str(peer).map_err(|error| cluster_error(error.into()))?;
    cluster
        .run_job(name, &peer, |control, command| {
            if !control {
                return Ok(command);
            }
            let (authority, _guard) = cluster.enter(&peer)?;
            let root = project_root(&cluster).map_err(io::Error::other)?;
            let mut local = Command::new(std::env::current_exe()?);
            local
                .args(command.get_args())
                .current_dir(&root)
                .process_group(0);
            bind(&mut local, &root, &authority).map_err(io::Error::other)?;
            Ok(local)
        })
        .map_err(cluster_error)
}

pub(crate) fn bind(
    command: &mut Command,
    root: &Path,
    authority: &Authority,
) -> Result<(), CliError> {
    let peer =
        serde_json::to_string(authority.peer()).map_err(|error| cluster_error(error.into()))?;
    command
        .env(CLUSTER, authority.cluster().root())
        .env(PROJECT, root)
        .env(SESSION, peer);
    Ok(())
}

fn enter() -> Result<Option<(Authority, File)>, CliError> {
    let Some(peer) = std::env::var_os(SESSION) else {
        return Ok(None);
    };
    let peer = serde_json::from_str(&peer.to_string_lossy())
        .map_err(|error| cluster_error(error.into()))?;
    let directory = std::env::var_os(CLUSTER)
        .ok_or_else(|| CliError::Usage("missing runtime directory".to_owned()))?;
    let cluster = Cluster::open(Path::new(&directory)).map_err(cluster_error)?;
    cluster.enter(&peer).map(Some).map_err(cluster_error)
}

pub(crate) fn authority() -> Result<Option<Authority>, CliError> {
    Ok(enter()?.map(|(authority, _)| authority))
}
