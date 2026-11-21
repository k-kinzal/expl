//! [`Project`] — where an exploration lives on disk.
//!
//! A project is a git repository with two expl-owned locations, split the
//! way iter splits an `Iterfile` from `.iter/`:
//!
//! - the **declaration**, `expl.toml` at the repository root — what the
//!   operator wrote; safe to commit.
//! - the **state directory**, `.expl/` — the graph database, node
//!   worktrees, agent logs, and the run lock; local and never committed.
//!
//! Discovery is bounded by the git working tree: a project is found only at
//! the top level of the working tree that contains the start directory.
//! This matters when expl develops expl — an agent running inside a node's
//! worktree (itself a git working tree) cannot accidentally reach the
//! enclosing project's graph.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::git::{Git, GitError};

/// Locating or locking a project failed.
#[derive(Debug, Error)]
pub enum ProjectError {
    /// The start directory is not inside a git working tree.
    #[error("{0} is not inside a git repository")]
    NotARepository(String, #[source] GitError),
    /// The repository has not been initialised with `expl init`.
    #[error("no expl project at {0} (run `expl init`)")]
    NotInitialized(String),
    /// Another expl process holds the run lock.
    #[error("another expl process is running agents in this project (lock: {0})")]
    Locked(String),
    /// Filesystem failure.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// An expl project rooted at a git repository's top level.
#[derive(Debug, Clone)]
pub struct Project {
    root: PathBuf,
}

/// Exclusive right to run agents in a project, held for a command's
/// lifetime. Released on drop.
#[derive(Debug)]
pub struct ProjectLock {
    _file: File,
}

impl Project {
    /// Name of the declaration file at the project root.
    pub const DECLARATION_FILE: &'static str = "expl.toml";
    /// Name of the state directory at the project root.
    pub const STATE_DIR: &'static str = ".expl";

    /// A project rooted at `root` (no checks).
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The project for the git working tree containing `start`, which need
    /// not be initialised yet.
    ///
    /// # Errors
    ///
    /// [`ProjectError::NotARepository`] when `start` is outside git.
    pub fn for_repository(start: &Path) -> Result<Self, ProjectError> {
        let root = Git::new(start)
            .toplevel()
            .map_err(|e| ProjectError::NotARepository(start.display().to_string(), e))?;
        Ok(Self::at(root))
    }

    /// The initialised project for the git working tree containing `start`.
    ///
    /// # Errors
    ///
    /// [`ProjectError::NotARepository`] or [`ProjectError::NotInitialized`].
    pub fn discover(start: &Path) -> Result<Self, ProjectError> {
        let project = Self::for_repository(start)?;
        if project.is_initialized() {
            Ok(project)
        } else {
            Err(ProjectError::NotInitialized(
                project.root.display().to_string(),
            ))
        }
    }

    /// Whether `expl init` has run here.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.database_path().is_file()
    }

    /// Repository top level.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `expl.toml`.
    #[must_use]
    pub fn declaration_path(&self) -> PathBuf {
        self.root.join(Self::DECLARATION_FILE)
    }

    /// `.expl/`.
    #[must_use]
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(Self::STATE_DIR)
    }

    /// `.expl/expl.db` — the graph.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.state_dir().join("expl.db")
    }

    /// `.expl/logs/` — raw agent output, one file per agent run.
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir().join("logs")
    }

    /// `.expl/lock`.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.state_dir().join("lock")
    }

    /// Take the exclusive run lock without waiting.
    ///
    /// Commands that run agents hold it so that node claims, worktrees, and
    /// interrupted-run recovery have a single owner.
    ///
    /// # Errors
    ///
    /// [`ProjectError::Locked`] when another process holds it.
    pub fn lock(&self) -> Result<ProjectLock, ProjectError> {
        let path = self.lock_path();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| ProjectError::Io {
                context: format!("open {}", path.display()),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(ProjectLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err(ProjectError::Locked(path.display().to_string()))
            }
            Err(std::fs::TryLockError::Error(source)) => Err(ProjectError::Io {
                context: format!("lock {}", path.display()),
                source,
            }),
        }
    }
}
