//! [`Git`] — a thin, typed shell over the `git` executable.
//!
//! expl links every node to a git branch, so git is infrastructure for both
//! the [`project`](crate::project) (discovering the repository root) and the
//! [`workspace`](crate::workspace) (worktrees, merges, commits). This module
//! knows how to talk to git; it does not know what a node is.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

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

    /// Run git as a yes/no question: `true` on exit 0, `false` on exit 1.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git cannot start or exits with any other code.
    pub fn test(&self, args: &[&str]) -> Result<bool, GitError> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(GitError::Spawn)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(GitError::Failed {
                args: args.join(" "),
                dir: self.dir.display().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
        }
    }

    /// Whether `ancestor` is reachable from `descendant` (a commit is its
    /// own ancestor).
    ///
    /// # Errors
    ///
    /// [`GitError`] when either does not name a commit.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, GitError> {
        self.test(&["merge-base", "--is-ancestor", ancestor, descendant])
    }

    /// The branch checked out here, if not detached.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git cannot start.
    pub fn current_branch(&self) -> Result<Option<String>, GitError> {
        match self.run(&["symbolic-ref", "--quiet", "--short", "HEAD"]) {
            Ok(branch) if !branch.is_empty() => Ok(Some(branch)),
            Ok(_) | Err(GitError::Failed { .. }) => Ok(None),
            Err(error @ GitError::Spawn(_)) => Err(error),
        }
    }

    /// Whether a local branch named `name` exists.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git cannot start.
    pub fn has_branch(&self, name: &str) -> Result<bool, GitError> {
        let reference = format!("refs/heads/{name}");
        self.test(&["show-ref", "--verify", "--quiet", &reference])
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

    /// Paths `git status --porcelain` reports in the working tree: staged,
    /// modified, deleted, renamed (both names), conflicted, or untracked.
    /// Ignored files are not reported.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git fails.
    pub fn status_paths(&self) -> Result<Vec<String>, GitError> {
        let out = self.run(&["status", "--porcelain=v2", "-z", "--untracked-files=all"])?;
        let mut records = out.split('\0').filter(|record| !record.is_empty());
        let mut paths = Vec::new();
        while let Some(record) = records.next() {
            // Fields before the path: ordinary 8, renamed/copied 9,
            // unmerged 10, untracked 1.
            let fields = match record.as_bytes().first() {
                Some(b'1') => 8,
                Some(b'2') => 9,
                Some(b'u') => 10,
                Some(b'?') => 1,
                _ => continue,
            };
            if let Some(path) = record.splitn(fields + 1, ' ').nth(fields) {
                paths.push(path.to_owned());
            }
            if fields == 9
                && let Some(original) = records.next()
            {
                paths.push(original.to_owned());
            }
        }
        Ok(paths)
    }

    /// Paths that differ between the trees of commits `from` and `to`.
    ///
    /// # Errors
    ///
    /// [`GitError`] when either does not name a commit.
    pub fn diff_paths(&self, from: &str, to: &str) -> Result<Vec<String>, GitError> {
        let out = self.run(&["diff", "--name-only", "-z", from, to])?;
        Ok(out
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Whether `revision` names a commit in this repository.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git cannot start or fails other than by not
    /// finding the commit.
    pub fn has_commit(&self, revision: &str) -> Result<bool, GitError> {
        let spec = format!("{revision}^{{commit}}");
        self.test(&["rev-parse", "--verify", "--quiet", &spec])
    }

    /// The tree that merging `theirs` into `ours` would produce, without
    /// touching any worktree or index; `None` when the merge conflicts.
    /// Histories without a common ancestor merge from the empty tree.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git cannot start or fails other than by a
    /// conflict.
    pub fn merge_tree(&self, ours: &str, theirs: &str) -> Result<Option<String>, GitError> {
        let args = [
            "merge-tree",
            "--write-tree",
            "--allow-unrelated-histories",
            ours,
            theirs,
        ];
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(GitError::Spawn)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        match (output.status.code(), stdout.lines().next()) {
            (Some(0), Some(tree)) if !tree.trim().is_empty() => Ok(Some(tree.trim().to_owned())),
            (Some(1), _) => Ok(None),
            _ => Err(GitError::Failed {
                args: args.join(" "),
                dir: self.dir.display().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_paths_name_every_kind_of_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = Git::new(dir.path());
        git.run(&["init", "-q"]).expect("init");
        for (name, content) in [
            ("kept", "k"),
            ("edited", "e"),
            ("gone", "g"),
            ("moved", "m"),
        ] {
            std::fs::write(dir.path().join(name), content).expect("write");
        }
        std::fs::write(dir.path().join(".gitignore"), "ignored\n").expect("write");
        git.run(&["add", "."]).expect("add");
        git.run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "initial",
        ])
        .expect("commit");
        let base = git.resolve_commit("HEAD").expect("head");
        assert!(git.status_paths().expect("status").is_empty());

        std::fs::write(dir.path().join("edited"), "changed").expect("write");
        std::fs::remove_file(dir.path().join("gone")).expect("remove");
        git.run(&["mv", "moved", "renamed"]).expect("mv");
        std::fs::create_dir(dir.path().join("new dir")).expect("mkdir");
        std::fs::write(dir.path().join("new dir/with space"), "n").expect("write");
        std::fs::write(dir.path().join("ignored"), "i").expect("write");
        let mut paths = git.status_paths().expect("status");
        paths.sort();
        assert_eq!(
            paths,
            ["edited", "gone", "moved", "new dir/with space", "renamed"]
        );
        assert_eq!(
            git.diff_paths(&base, "HEAD").expect("diff"),
            Vec::<String>::new()
        );
    }
}
