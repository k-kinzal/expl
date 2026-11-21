//! [`Project`] — where an exploration lives on disk.
//!
//! A project is a git repository with two expl-owned locations, split the
//! way iter splits an `Iterfile` from `.iter/`:
//!
//! - the **declaration**, `expl.toml` at the repository root — what the
//!   operator wrote; safe to commit.
//! - the **state directory**, `.expl/` — the database (graph, events,
//!   processes), worktrees, and logs; local and never committed.
//!
//! Discovery is bounded by the git working tree: a project is found only at
//! the top level of the working tree that contains the start directory.
//! This matters when expl develops expl — an agent running inside a node's
//! worktree (itself a git working tree) cannot accidentally reach the
//! enclosing project's graph.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::git::{Git, GitError};

/// Locating a project failed.
#[derive(Debug, Error)]
pub enum ProjectError {
    /// The start directory is not inside a git working tree.
    #[error("{0} is not inside a git repository")]
    NotARepository(String, #[source] GitError),
    /// The repository has not been initialised with `expl init`.
    #[error("no expl project at {0} (run `expl init`)")]
    NotInitialized(String),
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

    /// `.expl/proc/` — one log file per process.
    #[must_use]
    pub fn processes_dir(&self) -> PathBuf {
        self.state_dir().join("proc")
    }

    /// `.expl/worktrees/` — node and scratch worktrees.
    #[must_use]
    pub fn worktrees_dir(&self) -> PathBuf {
        self.state_dir().join("worktrees")
    }

    /// `.expl/git.lock` — serialises git operations that touch shared
    /// repository state across processes.
    #[must_use]
    pub fn git_lock_path(&self) -> PathBuf {
        self.state_dir().join("git.lock")
    }
}
