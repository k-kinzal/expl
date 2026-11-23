//! [`AgentClone`] — the private repository an agent works in.

use std::path::{Path, PathBuf};
use tempfile::TempDir;
use thiserror::Error;

use crate::git::{Git, GitError};
/// Creating, resetting or reading a private clone failed.
#[derive(Debug, Error)]
pub enum CloneError {
    /// Git command failure.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// What an [`AgentClone`] starts as.
#[derive(Debug, Clone)]
pub struct Start {
    /// The commit checked out.
    pub commit: String,
    /// The branch HEAD is attached to there, or `None` for a detached HEAD.
    pub branch: Option<String>,
    /// A commit merged in on top, its conflicts left in the working tree
    /// for the agent to resolve.
    pub merging: Option<String>,
}

/// A repository of the agent's own, for one invocation: the project's
/// whole history through its objects (shared, never written), the tags,
/// and one branch or a detached HEAD at the commit the work starts from.
///
/// Whatever the agent does with git happens here — commits, branches,
/// resets, rewritten history — and stays here: the project's refs, its
/// configuration and its worktrees are outside the clone. What the agent
/// produced is read from the clone's files by the project's own git
/// ([`snapshot`](Self::snapshot)), never through the clone's `.git`, which
/// the agent controls. Removed on drop.
#[derive(Debug)]
pub struct AgentClone {
    _directory: TempDir,
    /// The project's repository, which the clone reads from.
    repository: Git,
    path: PathBuf,
    /// The index [`snapshot`](Self::snapshot) reads the files through,
    /// outside the clone.
    index: PathBuf,
    start: Start,
}

impl AgentClone {
    /// A new clone of `repository` under `root`, set up as `start`.
    ///
    /// # Errors
    ///
    /// [`CloneError::Git`] when git fails; [`CloneError::Io`] on
    /// filesystem failure.
    pub fn create(
        repository: &Git,
        root: &Path,
        label: &str,
        start: Start,
    ) -> Result<Self, CloneError> {
        if label.contains('/') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "clone label must be a file name",
            )
            .into());
        }
        std::fs::create_dir_all(root)?;
        let root = std::path::absolute(root)?;
        let directory = tempfile::Builder::new()
            .prefix(&format!("clone-{label}-"))
            .tempdir_in(root)?;
        let clone = Self {
            path: directory.path().join("checkout"),
            index: directory.path().join("index"),
            _directory: directory,
            repository: repository.clone(),
            start,
        };
        clone.build()?;
        Ok(clone)
    }

    /// The clone's directory, where the agent runs.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Put the clone back as it was created, whatever the agent left in
    /// it.
    ///
    /// # Errors
    ///
    /// As [`create`](Self::create).
    pub fn reset(&self) -> Result<(), CloneError> {
        remove(&self.path)?;
        self.build()
    }

    /// The tree of the files the clone holds now, as the project's git
    /// reads them: everything not ignored, and every path `base` tracks
    /// (even one an ignore rule matches), as it is on disk. Paths matching
    /// a pathspec in `keep` are left as they are in `base`, whatever the
    /// agent did to them.
    ///
    /// The tree is written to the project's objects.
    ///
    /// # Errors
    ///
    /// [`CloneError::Git`] when git fails.
    pub fn snapshot(&self, base: &str, keep: &[String]) -> Result<String, CloneError> {
        remove_file(&self.index)?;
        let result = self.snapshot_through_index(base, keep);
        remove_file(&self.index)?;
        result
    }

    fn snapshot_through_index(&self, base: &str, keep: &[String]) -> Result<String, CloneError> {
        let git = self.repository.reading(&self.path, &self.index)?;
        git.run(&["read-tree", base])?;
        git.run(&["add", "-A"])?;
        if !keep.is_empty() {
            let mut args = vec!["diff-index", "--cached", "--name-only", "-z", base, "--"];
            args.extend(keep.iter().map(String::as_str));
            let changed = git.run(&args)?;
            let changed: Vec<&str> = changed.split('\0').filter(|p| !p.is_empty()).collect();
            if !changed.is_empty() {
                keep_as_in(&git, base, &changed)?;
            }
        }
        Ok(git.run(&["write-tree"])?)
    }

    fn build(&self) -> Result<(), CloneError> {
        let path = self.path.display().to_string();
        Git::new(self.path.parent().unwrap_or(&self.path)).run(&["init", "-q", &path])?;
        let objects = self.repository.objects_dir()?.canonicalize()?;
        let alternates = self.path.join(".git/objects/info/alternates");
        std::fs::write(&alternates, format!("{}\n", objects.display()))?;

        // The tags and the one branch, written in one go: the objects are
        // all there already.
        let start = &self.start;
        let mut refs = self.repository.run(&[
            "for-each-ref",
            "--format=create %(refname) %(objectname)",
            "refs/tags/",
        ])?;
        if !refs.is_empty() {
            refs.push('\n');
        }
        if let Some(branch) = &start.branch {
            refs.extend(["create refs/heads/", branch, " ", &start.commit, "\n"]);
        }
        let git = Git::in_worktree(&self.path);
        if !refs.is_empty() {
            git.run_with_input(&["update-ref", "--stdin"], &refs)?;
        }
        match &start.branch {
            Some(branch) => {
                git.run(&["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")])?;
            }
            None => {
                git.run(&["update-ref", "--no-deref", "HEAD", &start.commit])?;
            }
        }
        git.run(&["reset", "-q", "--hard"])?;
        if let Some(commit) = &start.merging {
            let identity = identity_args(&git)?;
            let mut args: Vec<&str> = identity.iter().map(String::as_str).collect();
            args.extend(["merge", "--no-ff", "--no-edit", commit.as_str()]);
            match git.run(&args) {
                Ok(_) => {}
                Err(error @ GitError::Failed { .. }) => {
                    if git.unmerged_paths()?.is_empty() {
                        return Err(error.into());
                    }
                }
                Err(error @ GitError::Spawn(_)) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

/// Set each of `paths` in `git`'s index back to its entry in `base`, or
/// remove it where `base` has none.
fn keep_as_in(git: &Git, base: &str, paths: &[&str]) -> Result<(), CloneError> {
    let mut args = vec![
        "--literal-pathspecs",
        "ls-tree",
        "-z",
        "--full-tree",
        base,
        "--",
    ];
    args.extend(paths);
    let listed = git.run(&args)?;
    let mut entries = Vec::new();
    for record in listed.split('\0').filter(|r| !r.is_empty()) {
        // `<mode> SP <type> SP <object> TAB <path>`
        if let Some((meta, path)) = record.split_once('\t')
            && let [mode, _, object] = meta.split(' ').collect::<Vec<_>>()[..]
        {
            entries.push((path.to_owned(), format!("{mode},{object},{path}")));
        }
    }
    // `--add`: an entry the agent removed comes back.
    let mut update: Vec<&str> = vec!["update-index", "--add"];
    for (_, info) in &entries {
        update.extend(["--cacheinfo", info.as_str()]);
    }
    let gone: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|path| !entries.iter().any(|(listed, _)| listed == path))
        .collect();
    if !gone.is_empty() {
        update.extend(["--force-remove", "--"]);
        update.extend(gone);
    }
    if !entries.is_empty() || update.contains(&"--force-remove") {
        git.run(&update)?;
    }
    Ok(())
}

/// `-c user.name=… -c user.email=…` for whichever identity git has none
/// configured for, so expl's own commits and merges never fail for want of
/// one.
///
/// # Errors
///
/// Git could not be launched to read its configuration.
pub fn identity_args(git: &Git) -> Result<Vec<String>, CloneError> {
    let configured = |key: &str| match git.run(&["config", "--get", key]) {
        Ok(value) => Ok(!value.is_empty()),
        Err(GitError::Failed { .. }) => Ok(false),
        Err(error @ GitError::Spawn(_)) => Err(error),
    };
    let mut args = Vec::new();
    if !configured("user.name")? {
        args.extend(["-c".to_owned(), "user.name=expl".to_owned()]);
    }
    if !configured("user.email")? {
        args.extend(["-c".to_owned(), "user.email=expl@localhost".to_owned()]);
    }
    Ok(args)
}

fn remove(path: &Path) -> Result<(), CloneError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_file(path: &Path) -> Result<(), CloneError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
