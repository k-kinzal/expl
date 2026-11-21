//! [`Workspaces`], [`Workspace`], and [`ScratchWorkspace`].

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::git::{Git, GitError};
use crate::graph::{Node, NodeId};
use crate::project::Project;
use crate::workspace::WorkspaceError;

/// Distinguishes scratch worktrees made by one process.
static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The worktrees of one project. See the [module docs](crate::workspace).
#[derive(Debug, Clone)]
pub struct Workspaces {
    repository: Git,
    root: PathBuf,
    lock: PathBuf,
}

/// One step's own worktree, on the step's branch.
#[derive(Debug, Clone)]
pub struct Workspace {
    node: NodeId,
    path: PathBuf,
    branch: String,
    lock: PathBuf,
    /// Files left conflicted by joining predecessor branches.
    conflicts: Vec<String>,
}

/// A private, detached worktree for one read-only reaction; removed on
/// drop.
#[derive(Debug)]
pub struct ScratchWorkspace {
    repository: Git,
    path: PathBuf,
    lock: PathBuf,
}

/// Holds the cross-process git lock until dropped.
struct GitLock {
    _file: File,
}

fn lock(path: &Path) -> Result<GitLock, WorkspaceError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    file.lock()?;
    Ok(GitLock { _file: file })
}

impl Workspaces {
    /// The worktrees of `project`.
    #[must_use]
    pub fn new(project: &Project) -> Self {
        Self {
            repository: Git::new(project.root()),
            root: project.worktrees_dir(),
            lock: project.git_lock_path(),
        }
    }

    /// The branch that holds `node`.
    #[must_use]
    pub fn branch_name(node: NodeId) -> String {
        format!("expl/node-{}", node.0)
    }

    /// Where `node`'s own worktree lives.
    #[must_use]
    pub fn path_of(&self, node: NodeId) -> PathBuf {
        self.root.join(format!("node-{}", node.0))
    }

    /// A fresh worktree for `node`, starting from its predecessors. Any
    /// previous worktree and branch of the node (an earlier attempt) are
    /// discarded first.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::PredecessorNotRealized`] when a predecessor has no
    /// commit; [`WorkspaceError::Git`] when git fails other than by a merge
    /// conflict.
    pub fn create(&self, node: &Node, predecessors: &[Node]) -> Result<Workspace, WorkspaceError> {
        let mut commits = Vec::new();
        for predecessor in predecessors {
            let commit = predecessor
                .head_commit
                .clone()
                .ok_or(WorkspaceError::PredecessorNotRealized(predecessor.id))?;
            if !commits.contains(&commit) {
                commits.push(commit);
            }
        }
        let Some((base, joins)) = commits.split_first() else {
            return Err(WorkspaceError::PredecessorNotRealized(node.id));
        };

        let path = self.path_of(node.id);
        let branch = Self::branch_name(node.id);
        {
            let _guard = lock(&self.lock)?;
            self.discard_locked(node.id)?;
            std::fs::create_dir_all(&self.root)?;
            let path_text = path.display().to_string();
            self.repository
                .run(&["worktree", "add", "-b", &branch, &path_text, base])?;
        }

        let git = Git::new(&path);
        let identity = identity_args(&git)?;
        let mut conflicts = Vec::new();
        for commit in joins {
            let mut args: Vec<&str> = identity.iter().map(String::as_str).collect();
            args.extend(["merge", "--no-ff", "--no-edit", commit.as_str()]);
            match git.run(&args) {
                Ok(_) => {}
                Err(error @ GitError::Failed { .. }) => {
                    let unmerged = git.unmerged_paths()?;
                    if unmerged.is_empty() {
                        return Err(error.into());
                    }
                    // Leave the conflict for the agent; further joins cannot
                    // start until this merge is concluded.
                    conflicts = unmerged;
                    break;
                }
                Err(error @ GitError::Spawn(_)) => return Err(error.into()),
            }
        }
        Ok(Workspace {
            node: node.id,
            path,
            branch,
            lock: self.lock.clone(),
            conflicts,
        })
    }

    /// A private detached worktree at `node`'s commit.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NoCommit`] when the node was never realised.
    pub fn scratch(&self, node: &Node) -> Result<ScratchWorkspace, WorkspaceError> {
        let commit = node
            .head_commit
            .as_deref()
            .ok_or(WorkspaceError::NoCommit(node.id))?;
        let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let path = self.root.join(format!(
            "scratch-{}-{}-{sequence}",
            node.id.0,
            std::process::id()
        ));
        let _guard = lock(&self.lock)?;
        std::fs::create_dir_all(&self.root)?;
        let path_text = path.display().to_string();
        self.repository
            .run(&["worktree", "add", "--detach", &path_text, commit])?;
        Ok(ScratchWorkspace {
            repository: self.repository.clone(),
            path,
            lock: self.lock.clone(),
        })
    }

    /// Remove `node`'s worktree and branch, if present.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn discard(&self, node: NodeId) -> Result<(), WorkspaceError> {
        let _guard = lock(&self.lock)?;
        self.discard_locked(node)
    }

    fn discard_locked(&self, node: NodeId) -> Result<(), WorkspaceError> {
        let path = self.path_of(node);
        if path.exists() {
            let path_text = path.display().to_string();
            self.repository
                .run(&["worktree", "remove", "--force", &path_text])?;
        }
        self.repository.run(&["worktree", "prune"])?;
        let branch = Self::branch_name(node);
        let reference = format!("refs/heads/{branch}");
        match self
            .repository
            .run(&["show-ref", "--verify", "--quiet", &reference])
        {
            Ok(_) => {
                self.repository.run(&["branch", "-D", &branch])?;
            }
            Err(GitError::Failed { .. }) => {}
            Err(error @ GitError::Spawn(_)) => return Err(error.into()),
        }
        Ok(())
    }
}

impl Workspace {
    /// The node this worktree realises.
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// Worktree directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Branch checked out in it.
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Files left conflicted when predecessor branches were joined.
    #[must_use]
    pub fn conflicts(&self) -> &[String] {
        &self.conflicts
    }

    /// Commit everything in the worktree as the branch tip and return the
    /// tip commit. Commits nothing when the tree is clean (the tip is
    /// still returned).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::UnresolvedConflicts`] when conflicts remain.
    pub fn commit(&self, message: &str) -> Result<String, WorkspaceError> {
        let git = Git::new(&self.path);
        let unmerged = git.unmerged_paths()?;
        if !unmerged.is_empty() {
            return Err(WorkspaceError::UnresolvedConflicts(unmerged));
        }
        git.run(&["add", "-A"])?;
        let dirty = !git.run(&["status", "--porcelain"])?.is_empty();
        let merging = git
            .run(&["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .is_ok();
        if dirty || merging {
            let identity = identity_args(&git)?;
            let mut args: Vec<&str> = identity.iter().map(String::as_str).collect();
            args.extend(["commit", "-m", message]);
            let _guard = lock(&self.lock)?;
            git.run(&args)?;
        }
        Ok(git.run(&["rev-parse", "HEAD"])?)
    }
}

impl ScratchWorkspace {
    /// Worktree directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchWorkspace {
    fn drop(&mut self) {
        // Best effort: a leftover scratch worktree is pruned by the next one.
        if let Ok(_guard) = lock(&self.lock) {
            let path = self.path.display().to_string();
            self.repository
                .run(&["worktree", "remove", "--force", &path])
                .ok();
            self.repository.run(&["worktree", "prune"]).ok();
        }
    }
}

/// `-c user.name=… -c user.email=…` when the repository has no identity
/// configured, so expl's own commits never fail on a bare machine.
fn identity_args(git: &Git) -> Result<Vec<String>, WorkspaceError> {
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
