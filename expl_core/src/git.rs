//! [`Git`] — a thin, typed shell over the `git` executable.
//!
//! expl links every node to a git branch, so git is infrastructure for both
//! the [`project`](crate::project) (discovering the repository root) and the
//! [`workspace`](crate::workspace) (worktrees, merges, commits). This module
//! knows how to talk to git; it does not know what a node is.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use thiserror::Error;

/// Serialises git invocations that mutate shared repository state (refs,
/// the worktree list). Parallel executors each own a worktree, but branch
/// creation and worktree registration touch the common `.git` directory.
static REPOSITORY_MUTATION: Mutex<()> = Mutex::new(());

/// A git invocation failed.
#[derive(Debug, Error)]
pub enum GitError {
    /// The `git` executable could not be started.
    #[error("failed to start git: {0}")]
    Spawn(#[source] std::io::Error),
    /// git exited unsuccessfully.
    #[error("git {args} failed in {dir}: {stderr}")]
    Failed {
        /// The arguments passed to git, space-joined.
        args: String,
        /// The directory git ran in.
        dir: String,
        /// Trimmed stderr.
        stderr: String,
    },
}

/// git, bound to one working directory.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
}

impl Git {
    /// git operating in `dir`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory this git runs in.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Run git and return trimmed stdout.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git cannot start or exits non-zero.
    pub fn run(&self, args: &[&str]) -> Result<String, GitError> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(GitError::Spawn)?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(GitError::Failed {
                args: args.join(" "),
                dir: self.dir.display().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }

    /// Run git while holding the repository-mutation lock.
    ///
    /// # Errors
    ///
    /// Same as [`Git::run`].
    pub fn run_exclusive(&self, args: &[&str]) -> Result<String, GitError> {
        let _guard = REPOSITORY_MUTATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.run(args)
    }

    /// The top-level directory of the working tree containing `dir`.
    ///
    /// # Errors
    ///
    /// [`GitError`] when `dir` is not inside a git working tree.
    pub fn toplevel(&self) -> Result<PathBuf, GitError> {
        self.run(&["rev-parse", "--show-toplevel"])
            .map(PathBuf::from)
    }

    /// Resolve a revision to a full commit id.
    ///
    /// # Errors
    ///
    /// [`GitError`] when the revision does not name a commit.
    pub fn resolve_commit(&self, revision: &str) -> Result<String, GitError> {
        let spec = format!("{revision}^{{commit}}");
        self.run(&["rev-parse", "--verify", "--quiet", &spec])
    }

    /// Paths with unresolved merge conflicts.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git fails.
    pub fn unmerged_paths(&self) -> Result<Vec<String>, GitError> {
        let out = self.run(&["diff", "--name-only", "--diff-filter=U"])?;
        Ok(out.lines().map(str::to_owned).collect())
    }
}
