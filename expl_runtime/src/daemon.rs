//! The daemon: one elected host that drives every explorer registered with
//! it.
//!
//! An explorer joins a daemon by registering an [`Explorer`] record: where
//! it lives, and its [`Registration`] — which events its consumers react
//! to, what is executed for one of them, and where. The record is all the
//! daemon needs. It starts the registered consumers' [workers](crate::worker),
//! restarts them after an election, forwards operations from any member to
//! the leader, and executes commands on its members. None of that is
//! application code: the general-purpose explorer and a specialized one
//! are hosted the same way, side by side.
//!
//! A binary becomes the daemon by routing three entry points here:
//! [`serve`] (the daemon itself), [`run_job`] (one queued command) and
//! [`run_worker`] (one worker of a registered consumer).

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Interrupt;
use crate::agent::Agent;
use crate::cluster::{self, Authority, Cluster, CommandSpec, Outcome, Peer};
use crate::command::run_supervised;
use crate::process::{ProcessId, ProcessOptions, ProcessRecord, Processes};
use crate::registration::{
    ConsumerRegistration, Registration, RegistrationError, RouterDeclaration, TaskRegistration,
};
use crate::store::{self, StoreError};
use crate::worker::{ActivityLog, Worker, WorkerError};

/// The elected daemon's session, for the processes it starts.
pub const SESSION: &str = "EXPL_RUNTIME_SESSION";
/// The daemon's directory, for the processes it starts.
pub const CLUSTER: &str = "EXPL_RUNTIME_CLUSTER";
/// The directory of the explorer an operation is about, for the processes
/// the daemon starts.
pub const PROJECT: &str = "EXPL_RUNTIME_PROJECT";
/// The directory a forwarded operation was typed in, so the leader can
/// resolve the relative paths among its arguments.
pub const ORIGIN: &str = "EXPL_RUNTIME_ORIGIN";
/// The pool of the explorer a worker serves, for the commands it executes:
/// the agents they invoke count against that pool's limit.
pub const POOL: &str = "EXPL_RUNTIME_POOL";

/// A daemon operation failed.
#[derive(Debug, Error)]
pub enum DaemonError {
    /// The daemon's directory, or a process it starts, failed.
    #[error("daemon: {0}")]
    Io(#[from] io::Error),
    /// An explorer's record cannot be read.
    #[error("explorer record {path}: {reason}")]
    Record {
        /// The file.
        path: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The name cannot be a file name in the registry.
    #[error("explorer name `{0}` must use letters, digits, '-' and '_'")]
    InvalidName(String),
    /// Another directory registered this name first.
    #[error("explorer `{name}` is already registered for {directory}")]
    NameTaken {
        /// The name.
        name: String,
        /// Where the registered explorer lives.
        directory: String,
    },
    /// No explorer has this name.
    #[error("no explorer named `{0}` is registered with this daemon")]
    UnknownExplorer(String),
    /// The explorer registers no such consumer.
    #[error("explorer `{explorer}` registers no consumer named `{consumer}`")]
    UnknownConsumer {
        /// The explorer.
        explorer: String,
        /// The consumer asked for.
        consumer: String,
    },
    /// The explorer still has workers.
    #[error("explorer `{name}` still has {workers} worker(s) running; stop them first")]
    Running {
        /// The explorer.
        name: String,
        /// How many of its workers are live.
        workers: usize,
    },
    /// What the explorer registers is inconsistent.
    #[error("explorer `{name}`: {source}")]
    Registration {
        /// The explorer.
        name: String,
        /// What is wrong.
        #[source]
        source: RegistrationError,
    },
    /// The explorer's database could not be opened.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The explorer's database failed.
    #[error("explorer database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A worker could not run.
    #[error(transparent)]
    Worker(#[from] WorkerError),
    /// The process was not started by an elected daemon.
    #[error("not running under an elected daemon")]
    NoAuthority,
}

/// One explorer as a daemon knows it: where it lives and what it registers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Explorer {
    /// The name it is registered and addressed by.
    pub name: String,
    /// Its root: the working directory of its commands.
    pub directory: PathBuf,
    /// Its database, holding its event log, deliveries and process table.
    pub database: PathBuf,
    /// Its state directory: reaction slots live here.
    pub state: PathBuf,
    /// Where its workers' logs go.
    pub logs: PathBuf,
    /// Program and leading arguments of its own command line, if it has
    /// one. Informational: what it registers to run names full commands.
    #[serde(default)]
    pub program: Vec<String>,
    /// A command the leader runs once after each election, before the
    /// explorer's workers are started again: the place to put right what a
    /// lost leader left half done.
    #[serde(default)]
    pub recover: Vec<String>,
    /// Base URL of the OTLP receiver its workers export to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<String>,
    /// Its consumers, tasks and routing.
    pub registration: Registration,
}

/// An explorer as written in a file by hand: an [`Explorer`] with its
/// registration's tables at the top level and paths relative to the file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplorerFile {
    name: String,
    #[serde(default)]
    directory: Option<PathBuf>,
    database: PathBuf,
    #[serde(default)]
    state: Option<PathBuf>,
    #[serde(default)]
    logs: Option<PathBuf>,
    #[serde(default)]
    program: Vec<String>,
    #[serde(default)]
    recover: Vec<String>,
    #[serde(default)]
    telemetry: Option<String>,
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    consumer: std::collections::BTreeMap<String, ConsumerRegistration>,
    #[serde(default)]
    router: std::collections::BTreeMap<String, RouterDeclaration>,
    #[serde(default)]
    routing: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    task: std::collections::BTreeMap<String, TaskRegistration>,
}

impl Explorer {
    /// Read an explorer from a TOML file (or JSON, when the file is named
    /// `*.json`). `directory` defaults to the file's directory and every
    /// other path is taken relative to it; `state` defaults to the
    /// database's directory and `logs` to `proc` inside the state
    /// directory.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Record`] when the file is unreadable or malformed.
    pub fn from_file(path: &Path) -> Result<Self, DaemonError> {
        let malformed = |reason: String| DaemonError::Record {
            path: path.display().to_string(),
            reason,
        };
        let text = fs::read_to_string(path).map_err(|error| malformed(error.to_string()))?;
        let file: ExplorerFile = if path.extension().is_some_and(|ext| ext == "json") {
            serde_json::from_str(&text).map_err(|error| malformed(error.to_string()))?
        } else {
            toml::from_str(&text).map_err(|error| malformed(error.to_string()))?
        };
        let base = std::path::absolute(path)
            .map_err(|error| malformed(error.to_string()))?
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        let directory = file
            .directory
            .map_or_else(|| base.clone(), |directory| base.join(directory));
        let database = directory.join(file.database);
        let state = file.state.map_or_else(
            || database.parent().unwrap_or(&directory).to_path_buf(),
            |state| directory.join(state),
        );
        let logs = file
            .logs
            .map_or_else(|| state.join("proc"), |logs| directory.join(logs));
        Ok(Self {
            name: file.name,
            directory,
            database,
            state,
            logs,
            program: file.program,
            recover: file.recover,
            telemetry: file.telemetry,
            registration: Registration {
                consumer: file.consumer,
                router: file.router,
                routing: file.routing,
                task: file.task,
                pool: file.pool,
            },
        })
    }

    /// The pool this explorer's work counts against: the one it registers,
    /// or one of its own, named after it.
    #[must_use]
    pub fn pool(&self) -> &str {
        self.registration.pool.as_deref().unwrap_or(&self.name)
    }

    /// The explorer's process table.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Store`] when its database cannot be opened.
    pub fn processes(&self) -> Result<Processes, DaemonError> {
        Ok(Processes::open(&self.database, &self.logs)?)
    }

    /// Create the framework's tables in the explorer's database when it
    /// has none yet, and its directories. `shared` marks the database as
    /// living on storage several machines use, as a daemon's explorers do.
    ///
    /// # Errors
    ///
    /// [`DaemonError`] on filesystem or database failure.
    pub fn prepare(&self, shared: Option<&Cluster>) -> Result<(), DaemonError> {
        for directory in [&self.state, &self.logs] {
            fs::create_dir_all(directory)?;
        }
        if let Some(directory) = self.database.parent() {
            fs::create_dir_all(directory)?;
            let marker = directory.join(store::CLUSTER_MARKER);
            if let Some(cluster) = shared
                && !marker.exists()
            {
                fs::write(&marker, cluster.root().to_string_lossy().as_bytes())?;
            }
        }
        let conn = store::open(&self.database)?;
        store::initialize(&conn)?;
        Ok(())
    }
}

/// The explorers registered with one daemon, kept in its directory so that
/// whichever member is elected finds them.
#[derive(Debug, Clone)]
pub struct Registry {
    cluster: Cluster,
}

impl Registry {
    /// The registry of `cluster`.
    #[must_use]
    pub fn of(cluster: &Cluster) -> Self {
        Self {
            cluster: cluster.clone(),
        }
    }

    fn directory(&self) -> PathBuf {
        self.cluster.root().join("explorers")
    }

    fn path(&self, name: &str) -> Result<PathBuf, DaemonError> {
        cluster::check_name(name).map_err(|_| DaemonError::InvalidName(name.to_owned()))?;
        Ok(self.directory().join(format!("{name}.json")))
    }

    /// One change of the registry at a time, across members.
    fn lock(&self) -> io::Result<File> {
        fs::create_dir_all(self.directory())?;
        let lock = cluster::lock_file(&self.cluster.root().join("explorers.lock"))?;
        lock.lock()?;
        Ok(lock)
    }

    /// Register `explorer`, or replace what it registered before. A name
    /// belongs to the directory that registered it first.
    ///
    /// # Errors
    ///
    /// [`DaemonError::NameTaken`] when another directory holds the name;
    /// [`DaemonError::Registration`] when what it registers is
    /// inconsistent; otherwise filesystem failure.
    pub fn join(&self, explorer: &Explorer) -> Result<(), DaemonError> {
        let path = self.path(&explorer.name)?;
        explorer
            .registration
            .validate()
            .map_err(|source| DaemonError::Registration {
                name: explorer.name.clone(),
                source,
            })?;
        let _lock = self.lock()?;
        if let Some(known) = Self::read(&path)?
            && known.directory != explorer.directory
        {
            return Err(DaemonError::NameTaken {
                name: explorer.name.clone(),
                directory: known.directory.display().to_string(),
            });
        }
        cluster::atomic_json(&path, explorer)?;
        Ok(())
    }

    /// Remove `name` from the registry and return what it had registered.
    ///
    /// # Errors
    ///
    /// [`DaemonError::UnknownExplorer`]; [`DaemonError::Running`] while
    /// workers of it are live.
    pub fn leave(&self, name: &str) -> Result<Explorer, DaemonError> {
        let path = self.path(name)?;
        let _lock = self.lock()?;
        let explorer =
            Self::read(&path)?.ok_or_else(|| DaemonError::UnknownExplorer(name.to_owned()))?;
        let workers = explorer.processes()?.list(false)?.len();
        if workers > 0 {
            return Err(DaemonError::Running {
                name: name.to_owned(),
                workers,
            });
        }
        fs::remove_file(&path)?;
        Ok(explorer)
    }

    /// The explorer registered as `name`, if any.
    ///
    /// # Errors
    ///
    /// [`DaemonError`] when the registry cannot be read.
    pub fn explorer(&self, name: &str) -> Result<Option<Explorer>, DaemonError> {
        let path = self.path(name)?;
        Self::read(&path)
    }

    /// The explorer registered as `name`.
    ///
    /// # Errors
    ///
    /// [`DaemonError::UnknownExplorer`] when there is none.
    pub fn expect(&self, name: &str) -> Result<Explorer, DaemonError> {
        self.explorer(name)?
            .ok_or_else(|| DaemonError::UnknownExplorer(name.to_owned()))
    }

    /// Every registered explorer, by name.
    ///
    /// # Errors
    ///
    /// [`DaemonError`] when the registry cannot be read.
    pub fn explorers(&self) -> Result<Vec<Explorer>, DaemonError> {
        let entries = match fs::read_dir(self.directory()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut explorers = Vec::new();
        for entry in entries {
            let path = entry?.path();
            // Records only: a record being written has another name.
            if path.extension().is_some_and(|ext| ext == "json")
                && let Some(explorer) = Self::read(&path)?
            {
                explorers.push(explorer);
            }
        }
        explorers.sort_by(|a: &Explorer, b: &Explorer| a.name.cmp(&b.name));
        Ok(explorers)
    }

    fn read(path: &Path) -> Result<Option<Explorer>, DaemonError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| DaemonError::Record {
                path: path.display().to_string(),
                reason: error.to_string(),
            })
    }
}

/// Give `command` the authority of the elected daemon, to work on the
/// explorer living in `directory`.
///
/// # Errors
///
/// The session cannot be encoded.
pub fn bind(command: &mut Command, directory: &Path, authority: &Authority) -> io::Result<()> {
    let peer = serde_json::to_string(authority.peer())?;
    command
        .env(CLUSTER, authority.cluster().root())
        .env(PROJECT, directory)
        .env(SESSION, peer);
    Ok(())
}

/// The authority this process was started under, if an elected daemon
/// started it, and the guard that keeps a successor waiting while the
/// process works. Keep the guard for as long as the explorer's state is
/// touched.
///
/// # Errors
///
/// The session is malformed, or the daemon's term is over.
pub fn enter() -> io::Result<Option<(Authority, File)>> {
    let Some(peer) = std::env::var_os(SESSION) else {
        return Ok(None);
    };
    let peer: Peer = serde_json::from_str(&peer.to_string_lossy())?;
    let directory = std::env::var_os(CLUSTER)
        .ok_or_else(|| io::Error::other("the daemon session names no daemon directory"))?;
    let cluster = Cluster::open(Path::new(&directory))?;
    cluster.enter(&peer).map(Some)
}

/// The directory of the explorer this process was started for by a daemon.
#[must_use]
pub fn bound_directory() -> Option<PathBuf> {
    std::env::var_os(PROJECT).map(PathBuf::from)
}

/// The directory a forwarded operation was typed in.
#[must_use]
pub fn origin() -> Option<PathBuf> {
    std::env::var_os(ORIGIN).map(PathBuf::from)
}

/// Put `agent` under the control of the daemon this process was started
/// by: its invocations run on the daemon's members, within the limits of
/// the daemon and of the explorer's pool. Outside a daemon the agent is
/// returned as it is. What a registered command calls before it invokes an
/// agent of its own.
///
/// # Errors
///
/// The daemon's directory cannot be opened.
pub fn govern(agent: Agent) -> io::Result<Agent> {
    let Some(directory) = std::env::var_os(CLUSTER) else {
        return Ok(agent);
    };
    let agent = agent.with_cluster(Cluster::open(Path::new(&directory))?);
    Ok(match std::env::var(POOL) {
        Ok(pool) => agent.in_pool(&pool),
        Err(_) => agent,
    })
}

/// Send an operation of the daemon binary through `via` to the leader,
/// which runs it for the explorer living in `directory`, and copy what it
/// prints. Returns the exit code to end with.
///
/// # Errors
///
/// Transport or output failure, or an operation the leader could not
/// finish. An operation that may have started is never replayed.
pub fn forward(
    cluster: &Cluster,
    via: &Peer,
    arguments: impl IntoIterator<Item = OsString>,
    directory: &Path,
    interrupt: &Interrupt,
) -> io::Result<u8> {
    // The leader runs its own binary with these arguments.
    let mut command = Command::new("expl");
    command.args(arguments);
    if let Ok(origin) = std::env::current_dir() {
        command.env(ORIGIN, origin);
    }
    let outcome = cluster.control(
        via,
        CommandSpec::capture(&command, directory),
        interrupt,
        &mut io::stdout(),
        &mut io::stderr(),
    )?;
    match outcome {
        Outcome::Exited(status) => {
            let status = ExitStatus::from_raw(status);
            Ok(status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(2))
        }
        Outcome::Interrupted if interrupt.is_raised() => Ok(130),
        Outcome::Interrupted => Err(io::Error::other("leader interrupted the operation")),
        Outcome::Failed(reason) => Err(io::Error::other(reason)),
        Outcome::TimedOut | Outcome::Lingering { .. } => {
            Err(io::Error::other("daemon operation did not finish"))
        }
    }
}

/// Supervise one queued command: the entry point of the process a daemon
/// starts for each job. A forwarded operation runs as this binary, in the
/// directory it names, under the leader's authority; a command for a member
/// runs as it is.
///
/// # Errors
///
/// Filesystem failure in the daemon's directory.
pub fn run_job(directory: &Path, name: &str, peer: &str) -> io::Result<()> {
    let cluster = Cluster::open(directory)?;
    let peer: Peer = serde_json::from_str(peer)?;
    cluster.run_job(name, &peer, |control, command| {
        if !control {
            return Ok(command);
        }
        let (authority, _guard) = cluster.enter(&peer)?;
        let directory = command
            .get_current_dir()
            .map_or_else(|| cluster.root().to_path_buf(), Path::to_path_buf);
        let mut local = Command::new(std::env::current_exe()?);
        local
            .args(command.get_args())
            .current_dir(&directory)
            .process_group(0);
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => local.env(key, value),
                None => local.env_remove(key),
            };
        }
        bind(&mut local, &directory, &authority)?;
        Ok(local)
    })
}

/// Run as a daemon of `cluster` until interrupted: a member that executes
/// commands and forwards operations, and, while elected, the leader that
/// drives every registered explorer. `exe` is this binary, which must route
/// its `runtime-job` and `runtime-worker` commands to [`run_job`] and
/// [`run_worker`]. `note` receives what an operator should know but that
/// does not stop the daemon (an explorer that could not be recovered).
///
/// # Errors
///
/// [`DaemonError`] when the daemon's directory fails or the member name is
/// taken.
pub fn serve(
    cluster: &Cluster,
    member: &str,
    exe: &Path,
    interrupt: &Interrupt,
    note: &dyn Fn(&str),
) -> Result<(), DaemonError> {
    let registry = Registry::of(cluster);
    let elected = std::cell::Cell::new(false);
    let mut workers: Vec<Child> = Vec::new();
    cluster.serve(
        member,
        interrupt,
        |_| {
            elected.set(true);
            Ok(())
        },
        |authority| {
            // Leadership is published now, so what is started here can
            // enter the term.
            if elected.replace(false) {
                recover(&registry, exe, authority, interrupt, &mut workers, note);
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
            Command::new(exe)
                .arg("--cluster")
                .arg(cluster.root())
                .arg("runtime-job")
                .arg(job)
                .arg(serde_json::to_string(peer)?)
                .current_dir(cluster.root())
                .process_group(0)
                .spawn()
        },
    )?;
    Ok(())
}

/// After an election: for each explorer, run its recover command, then
/// start again the workers the last leader had been asked to keep running.
/// One explorer that cannot be recovered does not hold up the others.
fn recover(
    registry: &Registry,
    exe: &Path,
    authority: &Authority,
    interrupt: &Interrupt,
    workers: &mut Vec<Child>,
    note: &dyn Fn(&str),
) {
    let explorers = match registry.explorers() {
        Ok(explorers) => explorers,
        Err(error) => {
            note(&format!("cannot read the registered explorers: {error}"));
            return;
        }
    };
    for explorer in explorers {
        let recovered = explorer
            .processes()
            .and_then(|mut table| Ok(table.recover()?));
        let recovered = match recovered {
            Ok(recovered) => recovered,
            Err(error) => {
                note(&format!("explorer `{}`: {error}", explorer.name));
                continue;
            }
        };
        if let Some((program, arguments)) = explorer.recover.split_first() {
            let mut hook = Command::new(program);
            hook.args(arguments).current_dir(&explorer.directory);
            let ran = bind(&mut hook, &explorer.directory, authority)
                .and_then(|()| run_supervised(hook, interrupt));
            match ran {
                Ok(output) if output.status.success() => {}
                Ok(output) => note(&format!(
                    "explorer `{}`: its recover command exited with {}: {}",
                    explorer.name,
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Err(error) => note(&format!(
                    "explorer `{}`: its recover command could not run: {error}",
                    explorer.name
                )),
            }
        }
        for (record, options) in recovered {
            match spawn_worker(exe, authority, &explorer, &record, options) {
                Ok(child) => workers.push(child),
                Err(error) => note(&format!(
                    "explorer `{}`: cannot restart {}: {error}",
                    explorer.name, record.name
                )),
            }
        }
    }
}

/// Start the worker recorded as `record` for a consumer of `explorer`: this
/// binary's `runtime-worker`, detached in its own process group, its output
/// going to the record's log. The child must adopt the record within its
/// startup deadline.
///
/// # Errors
///
/// The log cannot be opened or the process cannot be started.
pub fn spawn_worker(
    exe: &Path,
    authority: &Authority,
    explorer: &Explorer,
    record: &ProcessRecord,
    options: ProcessOptions,
) -> io::Result<Child> {
    fs::create_dir_all(&explorer.logs)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&record.log_path)?;
    let mut command = Command::new(exe);
    command
        .arg("--cluster")
        .arg(authority.cluster().root())
        .args(["runtime-worker", "--explorer", &explorer.name])
        .args(["--consumer", &record.consumer])
        .args(["--adopt", &record.id.0.to_string()])
        .current_dir(&explorer.directory)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0);
    if options.once {
        command.arg("--once");
    }
    if options.exit_when_idle {
        command.arg("--exit-when-idle");
    }
    bind(&mut command, &explorer.directory, authority)?;
    command.env(POOL, explorer.pool());
    // The child outlives this call; it is not waited for.
    command.spawn()
}

/// What [`up`] did about one consumer or record.
#[derive(Debug)]
pub enum Launch {
    /// A record nobody adopted in time gave its replica slot back.
    Abandoned(ProcessRecord),
    /// The consumer is not reachable through its delivery group.
    Unassigned(String),
    /// The consumer already has its workers.
    Running {
        /// The consumer.
        consumer: String,
        /// Its live workers.
        running: usize,
        /// The workers it registers.
        replicas: usize,
    },
    /// A worker was started.
    Started(ProcessRecord),
}

/// Start workers until each consumer of `explorer` (or each of `consumers`,
/// when given) has the `replicas` it registers. Idempotent: only what is
/// missing is started.
///
/// # Errors
///
/// [`DaemonError::UnknownConsumer`] for a name the explorer does not
/// register; otherwise database or process failure.
pub fn up(
    exe: &Path,
    authority: &Authority,
    explorer: &Explorer,
    consumers: &[String],
) -> Result<Vec<Launch>, DaemonError> {
    let registration = &explorer.registration;
    for name in consumers {
        if !registration.consumer.contains_key(name) {
            return Err(DaemonError::UnknownConsumer {
                explorer: explorer.name.clone(),
                consumer: name.clone(),
            });
        }
    }
    fs::create_dir_all(&explorer.logs)?;
    // One launch at a time for an explorer: each counts the live workers
    // before it starts the missing ones, and two counting at once would
    // both start them.
    fs::create_dir_all(&explorer.state)?;
    let launching = cluster::lock_file(&explorer.state.join("up.lock"))?;
    launching.lock()?;
    let mut table = explorer.processes()?;
    // A launch that never got adopted gives its slot back; its child, if
    // it ever arrives, can no longer adopt the record.
    let mut done: Vec<Launch> = table
        .abandon_unstarted()?
        .into_iter()
        .map(Launch::Abandoned)
        .collect();
    let live = table.list(false)?;
    for (name, consumer) in &registration.consumer {
        if !consumers.is_empty() && !consumers.contains(name) {
            continue;
        }
        if !registration.is_assigned(name) {
            done.push(Launch::Unassigned(name.clone()));
            continue;
        }
        let running = live.iter().filter(|r| &r.consumer == name).count();
        if running >= consumer.replicas {
            done.push(Launch::Running {
                consumer: name.clone(),
                running,
                replicas: consumer.replicas,
            });
            continue;
        }
        table.retire_dead(name)?;
        for _ in running..consumer.replicas {
            let record = table.create_resumable(name, None, ProcessOptions::default())?;
            if let Err(error) =
                spawn_worker(exe, authority, explorer, &record, ProcessOptions::default())
            {
                table.exit(record.id, &format!("failed to start: {error}"))?;
                return Err(error.into());
            }
            done.push(Launch::Started(record));
        }
    }
    Ok(done)
}

/// Be the worker recorded as `id` for `consumer` of `explorer`: the entry
/// point of the process [`spawn_worker`] starts. It holds no code of the
/// explorer; it executes what the explorer registered. Returns why it
/// exited.
///
/// # Errors
///
/// [`DaemonError::NoAuthority`] when not started by an elected daemon;
/// otherwise as [`Worker::run`].
pub fn run_worker(
    explorer: &Explorer,
    consumer: &str,
    id: ProcessId,
    options: ProcessOptions,
    interrupt: Interrupt,
) -> Result<String, DaemonError> {
    let (authority, _term) = enter()?.ok_or(DaemonError::NoAuthority)?;
    let record = explorer.processes()?.get(id)?;
    // Detached: stderr already is the worker's log.
    let observer = ActivityLog::new(&record.name, None);
    Ok(Worker {
        directory: &explorer.directory,
        database: &explorer.database,
        state: &explorer.state,
        logs: &explorer.logs,
        registration: &explorer.registration,
        consumer,
        id,
        interrupt,
        authority: Some(&authority),
    }
    .run(options, &observer)?)
}

/// Write `value` to `file` unless it already holds it; an existing file
/// with another value is an error.
///
/// # Errors
///
/// Filesystem failure, or [`ErrorKind::AlreadyExists`] when the file names
/// something else.
pub fn remember(file: &Path, value: &Path) -> io::Result<()> {
    let value = value
        .to_str()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "daemon paths must be UTF-8"))?;
    let parent = file
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "the file has no directory"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn explorer(root: &Path, name: &str) -> Explorer {
        let directory = root.join(name);
        Explorer {
            name: name.to_owned(),
            database: directory.join("state/explorer.db"),
            state: directory.join("state"),
            logs: directory.join("state/proc"),
            directory,
            program: Vec::new(),
            recover: Vec::new(),
            telemetry: None,
            registration: Registration::default(),
        }
    }

    #[test]
    fn a_name_belongs_to_the_directory_that_registered_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cluster = Cluster::open(&dir.path().join("cluster")).expect("cluster");
        let registry = Registry::of(&cluster);
        let mut first = explorer(dir.path(), "semx");
        registry.join(&first).expect("join");
        // The same explorer registering again replaces its record.
        first.registration.pool = Some("specialized".to_owned());
        registry.join(&first).expect("rejoin");
        assert_eq!(
            registry.expect("semx").expect("known").pool(),
            "specialized"
        );

        let mut other = explorer(dir.path(), "other");
        other.name = "semx".to_owned();
        let error = registry.join(&other).expect_err("taken");
        assert!(
            matches!(&error, DaemonError::NameTaken { name, .. } if name == "semx"),
            "{error}"
        );
        let second = explorer(dir.path(), "general");
        registry.join(&second).expect("a second explorer");
        let names: Vec<String> = registry
            .explorers()
            .expect("explorers")
            .into_iter()
            .map(|explorer| explorer.name)
            .collect();
        assert_eq!(names, ["general", "semx"]);
        for bad in ["", "a/b", "..", "with space"] {
            let mut invalid = explorer(dir.path(), "x");
            invalid.name = bad.to_owned();
            assert!(
                matches!(registry.join(&invalid), Err(DaemonError::InvalidName(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn an_explorer_with_workers_cannot_leave() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cluster = Cluster::open(&dir.path().join("cluster")).expect("cluster");
        let registry = Registry::of(&cluster);
        let explorer = explorer(dir.path(), "semx");
        explorer.prepare(Some(&cluster)).expect("prepare");
        registry.join(&explorer).expect("join");
        let mut table = explorer.processes().expect("processes");
        let record = table.create("measurer", None, true).expect("record");
        assert!(table.start(record.id, std::process::id()).expect("start"));
        let error = registry.leave("semx").expect_err("running");
        assert!(
            matches!(error, DaemonError::Running { workers: 1, .. }),
            "{error}"
        );
        table.exit(record.id, "stopped").expect("exit");
        assert_eq!(registry.leave("semx").expect("leave"), explorer);
        assert!(registry.explorer("semx").expect("read").is_none());
        assert!(matches!(
            registry.leave("semx"),
            Err(DaemonError::UnknownExplorer(_))
        ));
    }

    #[test]
    fn an_explorer_file_resolves_its_paths_against_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("explorer.toml");
        fs::write(
            &file,
            r#"
name = "semx"
database = ".semx/semx.db"
pool = "specialized"

[consumer.measurer]
on = ["candidate.implemented"]
run = ["./measure.sh"]
placement = "any"
replicas = 1

[task.sweep]
every = 60
run = ["./sweep.sh"]
"#,
        )
        .expect("file");
        let explorer = Explorer::from_file(&file).expect("explorer");
        let root = std::path::absolute(dir.path()).expect("absolute");
        assert_eq!(explorer.directory, root);
        assert_eq!(explorer.database, root.join(".semx/semx.db"));
        assert_eq!(explorer.state, root.join(".semx"));
        assert_eq!(explorer.logs, root.join(".semx/proc"));
        assert_eq!(explorer.pool(), "specialized");
        let measurer = &explorer.registration.consumer["measurer"];
        assert_eq!(measurer.on, ["candidate.implemented"]);
        assert_eq!(measurer.placement, crate::registration::Placement::Any);
        assert_eq!(explorer.registration.task["sweep"].every, 60);

        fs::write(
            &file,
            "name = \"semx\"\ndatabase = \"x.db\"\nsurprise = 1\n",
        )
        .expect("file");
        let error = Explorer::from_file(&file).expect_err("unknown key");
        assert!(matches!(error, DaemonError::Record { .. }), "{error}");
    }

    #[test]
    fn preparing_an_explorer_creates_its_tables_and_marks_shared_storage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cluster = Cluster::open(&dir.path().join("cluster")).expect("cluster");
        let explorer = explorer(dir.path(), "semx");
        explorer.prepare(Some(&cluster)).expect("prepare");
        explorer.prepare(Some(&cluster)).expect("again");
        assert!(explorer.state.join(store::CLUSTER_MARKER).is_file());
        let conn = store::open(&explorer.database).expect("open");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("mode");
        assert_eq!(mode.to_lowercase(), "delete");
        assert!(
            explorer
                .processes()
                .expect("table")
                .list(true)
                .expect("list")
                .is_empty()
        );
    }
}
