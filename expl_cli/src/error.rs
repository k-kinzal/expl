//! [`CliError`] and its exit-code categories (iter's table).

use expl_core::agent::AgentError;
use expl_core::git::GitError;
use expl_core::{
    DeclarationError, GraphError, ProjectError, RoleError, RunnerError, WorkspaceError,
};
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
    /// Project discovery or locking.
    #[error(transparent)]
    Project(#[from] ProjectError),
    /// `expl.toml`.
    #[error(transparent)]
    Declaration(#[from] DeclarationError),
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
    Role(#[from] RoleError),
    /// The runner.
    #[error(transparent)]
    Runner(#[from] RunnerError),
    /// Agent.
    #[error(transparent)]
    Agent(#[from] AgentError),
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
            | Self::Project(
                ProjectError::NotARepository(..)
                | ProjectError::NotInitialized(_)
                | ProjectError::Locked(_),
            )
            | Self::Graph(GraphError::NodeNotFound(_))
            | Self::Role(
                RoleError::NotApplicable { .. } | RoleError::Graph(GraphError::NodeNotFound(_)),
            )
            | Self::Runner(RunnerError::NoGoals) => exit::USER,
            Self::Declaration(_) | Self::Role(RoleError::Declaration(_)) => exit::CONFIG,
            Self::Project(ProjectError::Io { .. })
            | Self::Graph(_)
            | Self::Git(_)
            | Self::Workspace(_)
            | Self::Role(_)
            | Self::Runner(RunnerError::Graph(_))
            | Self::Agent(_)
            | Self::Io { .. } => exit::RUNTIME,
        }
    }
}
