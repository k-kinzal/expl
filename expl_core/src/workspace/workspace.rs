//! [`Workspaces`], [`Workspace`], and [`ScratchWorkspace`].

use std::collections::VecDeque;
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
    /// Every predecessor commit the branch starts from.
    predecessors: Vec<String>,
    /// The predecessor commit whose merge waits for its conflicts to be
    /// resolved.
    merging: Option<String>,
    /// Predecessor commits still to be merged after `merging`, in order.
    pending: VecDeque<String>,
    /// Files left conflicted by the merge of `merging`.
    conflicts: Vec<String>,
}

/// A private, detached worktree for one read-only reaction; removed on
/// drop.
#[derive(Debug)]
pub struct ScratchWorkspace {
    repository: Git,
    path: PathBuf,
    lock: PathBuf,
    /// The commit HEAD was detached at when the worktree was created.
    head: String,
}

/// What was changed in a [`ScratchWorkspace`] since it was created; empty
/// when it is unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScratchChanges {
    /// The commit the worktree was created at.
    pub created_at: String,
    /// The commit HEAD points at now, when it moved away from `created_at`.
    pub moved_to: Option<String>,
    /// Changed paths, sorted: those that differ between `created_at` and
    /// the moved HEAD, plus staged, modified, deleted, and untracked files.
    pub paths: Vec<String>,
}

impl ScratchChanges {
    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.moved_to.is_none() && self.paths.is_empty()
    }
}

impl std::fmt::Display for ScratchChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(moved_to) = &self.moved_to {
            write!(f, "HEAD moved from {} to {moved_to}", self.created_at)?;
            if !self.paths.is_empty() {
                f.write_str("; ")?;
            }
        }
        if !self.paths.is_empty() {
            write!(f, "changed paths: {}", self.paths.join(", "))?;
        } else if self.moved_to.is_none() {
            f.write_str("no changes")?;
        }
        Ok(())
    }
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

    /// A fresh worktree for `node`, starting from its predecessors: the
    /// first one's tip, merged with the others in order. Merging stops at
    /// the first conflict, which is left in the worktree
    /// ([`Workspace::conflicts`]); once it is resolved,
    /// [`Workspace::resume`] merges the remaining predecessors. Any
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

        let mut workspace = Workspace {
            node: node.id,
            path,
            branch,
            lock: self.lock.clone(),
            pending: joins.iter().cloned().collect(),
            predecessors: commits,
            merging: None,
            conflicts: Vec::new(),
        };
        workspace.merge_pending()?;
        Ok(workspace)
    }

    /// Of `commits`, the one whose history holds every other — the single
    /// state of the code they make up — if there is one.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn newest_of(&self, commits: &[String]) -> Result<Option<String>, WorkspaceError> {
        'candidates: for candidate in commits {
            for other in commits {
                if other != candidate && !self.repository.is_ancestor(other, candidate)? {
                    continue 'candidates;
                }
            }
            return Ok(Some(candidate.clone()));
        }
        Ok(None)
    }

    /// A private detached worktree at `node`'s commit.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NoCommit`] when the node was never realised;
    /// [`WorkspaceError::Git`] when git fails.
    pub fn scratch(&self, node: &Node) -> Result<ScratchWorkspace, WorkspaceError> {
        let commit = node
            .head_commit
            .as_deref()
            .ok_or(WorkspaceError::NoCommit(node.id))?;
        self.scratch_at(node.id, commit)
    }

    /// A private detached worktree at `commit`, for work about `node`.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn scratch_at(
        &self,
        node: NodeId,
        commit: &str,
    ) -> Result<ScratchWorkspace, WorkspaceError> {
        let commit = self.repository.resolve_commit(commit)?;
        let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let path = self.root.join(format!(
            "scratch-{}-{}-{sequence}",
            node.0,
            std::process::id()
        ));
        let _guard = lock(&self.lock)?;
        std::fs::create_dir_all(&self.root)?;
        let path_text = path.display().to_string();
        self.repository
            .run(&["worktree", "add", "--detach", &path_text, &commit])?;
        Ok(ScratchWorkspace {
            repository: self.repository.clone(),
            path,
            lock: self.lock.clone(),
            head: commit,
        })
    }

    /// The node worktrees registered with git: each node with the path its
    /// worktree is registered at, whether or not the directory still
    /// exists. Scratch worktrees are not node worktrees.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn registered(&self) -> Result<Vec<(NodeId, PathBuf)>, WorkspaceError> {
        let mut registered = Vec::new();
        for path in self.repository.worktrees()? {
            let node = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("node-"))
                .and_then(|id| id.parse().ok())
                .map(NodeId);
            if let Some(node) = node
                && path
                    .parent()
                    .is_some_and(|parent| same_dir(parent, &self.root))
            {
                registered.push((node, path));
            }
        }
        Ok(registered)
    }

    /// Drop the registration of the node worktree at `path`, keeping its
    /// branch: a registration whose directory is gone is pruned, and a
    /// worktree that still exists is removed only when it is clean and
    /// unlocked, so no uncommitted work is lost.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git refuses (the worktree has changes
    /// or is locked) or fails.
    pub fn unregister(&self, path: &Path) -> Result<(), WorkspaceError> {
        let _guard = lock(&self.lock)?;
        if path.exists() {
            let path_text = path.display().to_string();
            self.repository.run(&["worktree", "remove", &path_text])?;
        } else {
            self.repository.run(&["worktree", "prune"])?;
        }
        Ok(())
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

    /// Files left conflicted by the predecessor merge in progress; empty
    /// once every predecessor is merged.
    #[must_use]
    pub fn conflicts(&self) -> &[String] {
        &self.conflicts
    }

    /// Whether every predecessor is merged into the worktree.
    #[must_use]
    pub fn is_joined(&self) -> bool {
        self.merging.is_none() && self.pending.is_empty()
    }

    /// Conclude the predecessor merge whose [conflicts](Self::conflicts)
    /// were resolved in the worktree, then merge the remaining
    /// predecessors until the next conflict or until all are merged.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::UnresolvedConflicts`] when a conflicted file still
    /// holds conflict markers; [`WorkspaceError::MergeAbandoned`] when the
    /// merge was aborted instead of resolved; [`WorkspaceError::Git`] when
    /// git fails other than by a merge conflict.
    pub fn resume(&mut self) -> Result<(), WorkspaceError> {
        let Some(commit) = self.merging.clone() else {
            return self.merge_pending();
        };
        let git = Git::new(&self.path);
        let mut unresolved = Vec::new();
        for path in &self.conflicts {
            if has_conflict_markers(&self.path.join(path))? {
                unresolved.push(path.clone());
            }
        }
        if !unresolved.is_empty() {
            return Err(WorkspaceError::UnresolvedConflicts(unresolved));
        }

        let expected = git.resolve_commit(&commit)?;
        let merge_head = match git.run(&["rev-parse", "-q", "--verify", "MERGE_HEAD"]) {
            Ok(head) => Some(head),
            Err(GitError::Failed { .. }) => None,
            Err(error @ GitError::Spawn(_)) => return Err(error.into()),
        };
        if merge_head.as_deref() == Some(expected.as_str()) {
            // The merge commit holds what git merged plus the resolved
            // files, nothing else the resolving agent left behind.
            let mut add = vec!["add", "-A", "--"];
            add.extend(self.conflicts.iter().map(String::as_str));
            git.run(&add)?;
            let unmerged = git.unmerged_paths()?;
            if !unmerged.is_empty() {
                return Err(WorkspaceError::UnresolvedConflicts(unmerged));
            }
            let identity = identity_args(&git)?;
            let mut args: Vec<&str> = identity.iter().map(String::as_str).collect();
            args.extend(["commit", "--no-edit"]);
            let _guard = lock(&self.lock)?;
            git.run(&args)?;
            // Anything else is discarded: the step's own work starts after
            // the join, from the merged code.
            git.restore("HEAD")?;
        } else if !git.is_ancestor(&expected, "HEAD")? {
            return Err(WorkspaceError::MergeAbandoned(commit));
        }
        let unmerged = git.unmerged_paths()?;
        if !unmerged.is_empty() {
            return Err(WorkspaceError::UnresolvedConflicts(unmerged));
        }
        self.merging = None;
        self.conflicts.clear();
        self.merge_pending()
    }

    /// Check that every predecessor commit is an ancestor of `tip`.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::PredecessorNotMerged`] naming the first
    /// predecessor commit `tip` does not contain; [`WorkspaceError::Git`]
    /// when git fails.
    pub fn verify_predecessors(&self, tip: &str) -> Result<(), WorkspaceError> {
        let git = Git::new(&self.path);
        for commit in &self.predecessors {
            if !git.is_ancestor(commit, tip)? {
                return Err(WorkspaceError::PredecessorNotMerged {
                    commit: commit.clone(),
                    tip: tip.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Merge the pending predecessors in order, stopping at the first
    /// conflict and leaving it in the worktree.
    fn merge_pending(&mut self) -> Result<(), WorkspaceError> {
        let git = Git::new(&self.path);
        let identity = identity_args(&git)?;
        while let Some(commit) = self.pending.pop_front() {
            let mut args: Vec<&str> = identity.iter().map(String::as_str).collect();
            args.extend(["merge", "--no-ff", "--no-edit", commit.as_str()]);
            match git.run(&args) {
                Ok(_) => {}
                Err(error @ GitError::Failed { .. }) => {
                    let unmerged = git.unmerged_paths()?;
                    if unmerged.is_empty() {
                        return Err(error.into());
                    }
                    self.merging = Some(commit);
                    self.conflicts = unmerged;
                    return Ok(());
                }
                Err(error @ GitError::Spawn(_)) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Commit everything in the worktree as the branch tip and return the
    /// tip commit. Commits nothing when the tree is clean (the tip is
    /// still returned).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::JoinIncomplete`] while predecessors are not all
    /// merged; [`WorkspaceError::UnresolvedConflicts`] when conflicts
    /// remain.
    pub fn commit(&self, message: &str) -> Result<String, WorkspaceError> {
        if !self.is_joined() {
            return Err(WorkspaceError::JoinIncomplete(
                self.merging.iter().chain(&self.pending).cloned().collect(),
            ));
        }
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

    /// The commit HEAD was detached at when the worktree was created.
    #[must_use]
    pub fn head(&self) -> &str {
        &self.head
    }

    /// What changed since the worktree was created: whether HEAD moved,
    /// files changed, or untracked files appeared (ignored files aside).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn changes(&self) -> Result<ScratchChanges, WorkspaceError> {
        let git = Git::new(&self.path);
        let head = git.resolve_commit("HEAD")?;
        let mut paths = git.status_paths()?;
        let moved_to = if head == self.head {
            None
        } else {
            paths.extend(git.diff_paths(&self.head, &head)?);
            Some(head)
        };
        paths.sort();
        paths.dedup();
        Ok(ScratchChanges {
            created_at: self.head.clone(),
            moved_to,
            paths,
        })
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

/// Whether `a` and `b` name the same directory; git reports worktree paths
/// with symbolic links resolved.
fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Whether the file at `path` still holds a git conflict marker. A path
/// that is gone or is not a file was resolved by removing it.
fn has_conflict_markers(path: &Path) -> Result<bool, WorkspaceError> {
    if !path.is_file() {
        return Ok(false);
    }
    let content = std::fs::read(path)?;
    Ok(content.split(|&byte| byte == b'\n').any(|line| {
        [b"<<<<<<<", b"|||||||", b">>>>>>>"]
            .iter()
            .any(|marker| line.starts_with(*marker))
    }))
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
