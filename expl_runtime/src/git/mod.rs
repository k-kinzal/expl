//! [`Git`] — a thin, typed shell over the `git` executable.
//!
//! The command wrapper and private Git clones used by exploration applications.

mod agent_clone;

pub use agent_clone::{AgentClone, CloneError, Start, identity_args};

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

/// What stands in the way of a new branch
/// ([`Git::branch_blocker`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchBlocker {
    /// A branch of that name exists.
    Exists,
    /// A branch lives under the name (`name/…`): the one named.
    Nested(String),
    /// A parent path of the name is a branch: the one named.
    Parent(String),
}

/// git, bound to one working directory.
///
/// Inherited repository location overrides are removed from git subprocesses.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
    discovery: Discovery,
    /// An index file used in place of the repository's own
    /// ([`reading`](Self::reading)).
    index: Option<PathBuf>,
}

#[derive(Debug, Clone)]
enum Discovery {
    Default,
    Worktree,
    Below(OsString),
    /// The repository at this git directory, with `dir` as its working
    /// tree.
    Explicit(PathBuf),
}

impl Git {
    /// git operating in `dir`, with normal repository discovery.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            discovery: Discovery::Default,
            index: None,
        }
    }

    /// git rooted at `dir`, using its `.git` directory or worktree file.
    #[must_use]
    pub fn in_worktree(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            discovery: Discovery::Worktree,
            index: None,
        }
    }

    /// git discovering a repository below an encoded `GIT_CEILING_DIRECTORIES` path.
    #[must_use]
    pub fn below_ceiling(dir: PathBuf, ceiling: OsString) -> Self {
        Self {
            dir,
            discovery: Discovery::Below(ceiling),
            index: None,
        }
    }

    /// This repository reading the files of `work_tree` through the index
    /// file `index`: its own configuration, objects and refs, another
    /// directory's files. Whatever `work_tree` holds of git — a `.git` of
    /// its own, with its configuration and hooks — is never consulted.
    ///
    /// # Errors
    ///
    /// [`GitError`] when this repository's git directory cannot be found.
    pub(crate) fn reading(&self, work_tree: &Path, index: &Path) -> Result<Self, GitError> {
        let git_dir = self.run(&["rev-parse", "--absolute-git-dir"])?;
        Ok(Self {
            dir: work_tree.to_path_buf(),
            discovery: Discovery::Explicit(PathBuf::from(git_dir)),
            index: Some(index.to_path_buf()),
        })
    }

    /// The directory holding this repository's objects, shared by all of
    /// its worktrees.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git fails.
    pub(crate) fn objects_dir(&self) -> Result<PathBuf, GitError> {
        let common = PathBuf::from(self.run(&["rev-parse", "--git-common-dir"])?);
        let common = if common.is_relative() {
            self.dir.join(common)
        } else {
            common
        };
        Ok(common.join("objects"))
    }

    /// The directory this git runs in.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn output(&self, args: &[&str]) -> Result<Output, GitError> {
        self.command(args).output().map_err(GitError::Spawn)
    }

    /// Run git with `input` on its standard input and return trimmed
    /// stdout. For commands that read their instructions from stdin and
    /// write little (`update-ref --stdin`).
    ///
    /// # Errors
    ///
    /// [`GitError`] when git cannot start or exits non-zero.
    pub(crate) fn run_with_input(&self, args: &[&str], input: &str) -> Result<String, GitError> {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(GitError::Spawn)?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes()).map_err(GitError::Spawn)?;
        }
        let output = child.wait_with_output().map_err(GitError::Spawn)?;
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

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        // Start discovery and resolve Git's storage paths from the selected
        // checkout, even when Expl was launched from a Git hook.
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_IMPLICIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_GRAFT_FILE",
            "GIT_SHALLOW_FILE",
            "GIT_NAMESPACE",
            "GIT_REPLACE_REF_BASE",
            "GIT_PREFIX",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            command.env_remove(name);
        }
        match &self.discovery {
            Discovery::Default => {}
            Discovery::Worktree => {
                command.args(["--git-dir=.git", "--work-tree=."]);
            }
            Discovery::Below(ceiling) => {
                command.env("GIT_CEILING_DIRECTORIES", ceiling);
            }
            Discovery::Explicit(git_dir) => {
                let mut flag = OsString::from("--git-dir=");
                flag.push(git_dir);
                command.arg(flag).arg("--work-tree=.");
            }
        }
        if let Some(index) = &self.index {
            command.env("GIT_INDEX_FILE", index);
        }
        command
            .args(args)
            .current_dir(&self.dir)
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    /// Run git and return trimmed stdout.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git cannot start or exits non-zero.
    pub fn run(&self, args: &[&str]) -> Result<String, GitError> {
        let output = self.output(args)?;
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

    /// Run a Git transport under the worker's cancellation flag.
    ///
    /// # Errors
    ///
    /// Git failed, execution was interrupted, or its process group could not be reaped.
    pub fn network(&self, args: &[&str], interrupt: &crate::Interrupt) -> Result<(), GitError> {
        let output = crate::command::run_supervised(self.command(args), interrupt)
            .map_err(GitError::Spawn)?;
        if output.status.success() {
            Ok(())
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
        let output = self.output(args)?;
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

    /// The full ref HEAD is attached to (such as `refs/heads/main`), or
    /// `None` when HEAD is detached. Unlike
    /// [`current_branch`](Self::current_branch), a repository git cannot
    /// read is an error, not a detached HEAD.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git cannot start or fails other than by finding
    /// HEAD detached.
    pub fn head_ref(&self) -> Result<Option<String>, GitError> {
        self.read_symbolic_ref(&["symbolic-ref", "-q", "HEAD"])
    }

    /// The immediate target of `reference`, even when the target is
    /// missing. Returns `None` when `reference` is absent or direct.
    ///
    /// # Errors
    /// [`GitError`] when git cannot start or exits with an unexpected code.
    /// Read the destination of a symbolic ref, when it is symbolic.
    ///
    /// # Errors
    ///
    /// Git could not inspect this ref.
    pub fn symbolic_ref_target(&self, reference: &str) -> Result<Option<String>, GitError> {
        self.read_symbolic_ref(&["symbolic-ref", "-q", "--no-recurse", reference])
    }

    fn read_symbolic_ref(&self, args: &[&str]) -> Result<Option<String>, GitError> {
        let output = self.output(args)?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            )),
            Some(1) => Ok(None),
            _ => Err(GitError::Failed {
                args: args.join(" "),
                dir: self.dir.display().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
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

    /// The ref that stands in the way of a new local branch named `name`,
    /// if any: the branch itself, a branch under it (`name/…`), or a
    /// branch at one of its parent paths — git keeps none of these beside
    /// one another.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git cannot start or fails.
    pub fn branch_blocker(&self, name: &str) -> Result<Option<BranchBlocker>, GitError> {
        if self.has_branch(name)? {
            return Ok(Some(BranchBlocker::Exists));
        }
        let below = format!("refs/heads/{name}/");
        let nested = self.run(&[
            "for-each-ref",
            "--count=1",
            "--format=%(refname:short)",
            &below,
        ])?;
        if !nested.is_empty() {
            return Ok(Some(BranchBlocker::Nested(nested)));
        }
        for (slash, _) in name.match_indices('/') {
            let parent = &name[..slash];
            if self.has_branch(parent)? {
                return Ok(Some(BranchBlocker::Parent(parent.to_owned())));
            }
        }
        Ok(None)
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

    /// Put the worktree back at `head`: HEAD reset there, tracked files as
    /// they are in it, untracked files removed. Ignored files stay.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git fails.
    pub fn restore(&self, head: &str) -> Result<(), GitError> {
        self.run(&["reset", "-q", "--hard", head])?;
        self.run(&["clean", "-q", "-f", "-d"])?;
        Ok(())
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

    /// The paths of every worktree registered with the repository, the main
    /// one first, as git records them — whether or not their directories
    /// still exist.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git fails.
    pub fn worktrees(&self) -> Result<Vec<PathBuf>, GitError> {
        let out = self.run(&["worktree", "list", "--porcelain", "-z"])?;
        Ok(out
            .split('\0')
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect())
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
        let output = self.output(&args)?;
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
    fn restore_undoes_commits_edits_and_new_files_but_keeps_ignored_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = Git::new(dir.path());
        git.run(&["init", "-q"]).expect("init");
        std::fs::write(dir.path().join(".gitignore"), "target/\n").expect("write");
        std::fs::write(dir.path().join("kept"), "k").expect("write");
        git.run(&["add", "."]).expect("add");
        git.run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "base",
        ])
        .expect("commit");
        let head = git.resolve_commit("HEAD").expect("head");

        std::fs::write(dir.path().join("kept"), "changed").expect("write");
        std::fs::write(dir.path().join("extra"), "x").expect("write");
        git.run(&["add", "."]).expect("add");
        git.run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "moved",
        ])
        .expect("commit");
        std::fs::write(dir.path().join("kept"), "dirty").expect("write");
        std::fs::create_dir_all(dir.path().join("new/dir")).expect("dir");
        std::fs::write(dir.path().join("new/dir/file"), "n").expect("write");
        std::fs::create_dir_all(dir.path().join("target")).expect("dir");
        std::fs::write(dir.path().join("target/build"), "b").expect("write");

        git.restore(&head).expect("restore");
        assert_eq!(git.resolve_commit("HEAD").expect("head"), head);
        assert!(git.status_paths().expect("status").is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("kept")).expect("kept"),
            "k"
        );
        assert!(!dir.path().join("extra").exists() && !dir.path().join("new").exists());
        assert!(
            dir.path().join("target/build").exists(),
            "ignored files stay"
        );
    }

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
