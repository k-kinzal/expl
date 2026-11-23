//! [`CliError`] and its exit-code categories (iter's table).

use expl_core::agent::AgentError;
use expl_core::git::GitError;
use expl_core::react::ReactError;
use expl_core::{
    DeclarationError, DoctorError, GraphError, HandlerError, ProjectError, StoreError,
    WorkspaceError,
};
use expl_runtime::daemon::DaemonError;
use expl_runtime::worker::WorkerError;
use thiserror::Error;

/// Exit codes shared with iter.
pub(crate) mod exit {
    /// User input error: unknown target, wrong state, missing project.
    pub(crate) const USER: u8 = 1;
    /// Runtime failure: agent, git, database, I/O.
    pub(crate) const RUNTIME: u8 = 2;
    /// Configuration error: `expl.toml`.
    pub(crate) const CONFIG: u8 = 64;
}

/// A command failed.
#[derive(Debug, Error)]
pub(crate) enum CliError {
    /// The operator asked for something that cannot be done.
    #[error("{0}")]
    Usage(String),
    /// Project discovery.
    #[error(transparent)]
    Project(#[from] ProjectError),
    /// `expl.toml`.
    #[error(transparent)]
    Declaration(#[from] DeclarationError),
    /// The database file.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Process and delivery records.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// Graph.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// git.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Worktrees.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// A role.
    #[error(transparent)]
    Handler(#[from] HandlerError),
    /// A worker.
    #[error(transparent)]
    Process(#[from] WorkerError),
    /// The daemon and the explorers it hosts.
    #[error(transparent)]
    Daemon(#[from] DaemonError),
    /// One reaction, run for a worker.
    #[error(transparent)]
    React(#[from] ReactError),
    /// Agent.
    #[error(transparent)]
    Agent(#[from] AgentError),
    /// Project view for the window client.
    #[error(transparent)]
    Overview(expl_core::overview::OverviewError),
    /// Checking the graph against git.
    #[error(transparent)]
    Doctor(#[from] DoctorError),
    /// `expl doctor` found the graph and git disagreeing.
    #[error("the graph and git disagree in {0} place(s)")]
    Inconsistent(usize),
    /// Filesystem or terminal I/O.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl CliError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// The process exit code for this error.
    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_)
            | Self::Project(ProjectError::NotARepository(..) | ProjectError::NotInitialized(_))
            | Self::Declaration(DeclarationError::UnknownConsumer(_))
            | Self::Graph(
                GraphError::NodeNotFound(_)
                | GraphError::NotInStatus { .. }
                | GraphError::Cycle { .. },
            )
            | Self::Process(WorkerError::NotAdoptable(_))
            | Self::Daemon(
                DaemonError::InvalidName(_)
                | DaemonError::NameTaken { .. }
                | DaemonError::UnknownExplorer(_)
                | DaemonError::UnknownConsumer { .. }
                | DaemonError::Running { .. }
                | DaemonError::Record { .. }
                | DaemonError::Registration { .. },
            )
            | Self::Inconsistent(_) => exit::USER,
            Self::Declaration(_) | Self::Handler(HandlerError::Declaration(_)) => exit::CONFIG,
            Self::Project(ProjectError::Io { .. })
            | Self::Store(_)
            | Self::Database(_)
            | Self::Graph(_)
            | Self::Git(_)
            | Self::Workspace(_)
            | Self::Handler(_)
            | Self::Process(_)
            | Self::Daemon(_)
            | Self::React(_)
            | Self::Agent(_)
            | Self::Overview(_)
            | Self::Doctor(_)
            | Self::Io { .. } => exit::RUNTIME,
        }
    }
}
