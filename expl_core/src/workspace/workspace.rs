//! [`Workspaces`], [`Workspace`], and [`ScratchWorkspace`].

use std::collections::{HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::git::{BranchBlocker, Git, GitError};
use crate::graph::{Node, NodeId};
use crate::project::Project;
use crate::workspace::WorkspaceError;

/// Distinguishes scratch worktrees made by one process.
static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How many names [`Workspaces::create`] tries for a step's branch before
/// it gives up.
const BRANCH_ATTEMPTS: u32 = 100;

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
            repository: Git::in_worktree(project.root()),
            root: project.worktrees_dir(),
            lock: project.git_lock_path(),
        }
    }

    /// The name first tried for `node`'s branch. [`Workspaces::create`]
    /// moves on to a fresh name ([`Workspaces::branch_candidate`]) when a
    /// ref already takes it, so the branch a step is on is the one recorded
    /// on it ([`Workspace::branch`]), not necessarily this one.
    #[must_use]
    pub fn branch_name(node: NodeId) -> String {
        format!("expl/node-{}", node.0)
    }

    /// The `attempt`-th name tried for `node`'s branch, counting from 1:
    /// [`branch_name`](Self::branch_name) first, then `expl/node-<id>-2`,
    /// `expl/node-<id>-3`, and so on.
    #[must_use]
    pub fn branch_candidate(node: NodeId, attempt: u32) -> String {
        if attempt <= 1 {
            Self::branch_name(node)
        } else {
            format!("expl/node-{}-{attempt}", node.0)
        }
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
    /// [`Workspace::resume`] merges the remaining predecessors. A previous
    /// worktree of the node (an earlier attempt) is removed first.
    ///
    /// The step gets a branch of its own that did not exist before: names
    /// are tried in turn ([`branch_candidate`](Self::branch_candidate)),
    /// each created only if no ref is in its way, and one that git reports
    /// taken — by an earlier attempt of the step, an unrelated branch, or
    /// the trunk — is passed over for the next. Names among the `trunk`
    /// branches ([`Trunk::branches`](crate::trunk::Trunk::branches)) and
    /// their symbolic ref targets are passed over even when missing. No
    /// existing ref is ever replaced or deleted; the name taken is
    /// [`Workspace::branch`].
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::PredecessorNotRealized`] when a predecessor has no
    /// commit; [`WorkspaceError::NoFreeBranch`] when every name tried is
    /// taken; [`WorkspaceError::Git`] when git fails other than by a name
    /// collision or a merge conflict.
    pub fn create(
        &self,
        node: &Node,
        predecessors: &[Node],
        trunk: &[String],
    ) -> Result<Workspace, WorkspaceError> {
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

        let base = self.repository.resolve_commit(base)?;
        let path = self.path_of(node.id);
        let branch = {
            let _guard = lock(&self.lock)?;
            self.remove_worktree_locked(node.id)?;
            let branch = self.allocate_branch_locked(node.id, &base, trunk)?;
            std::fs::create_dir_all(&self.root)?;
            let path_text = path.display().to_string();
            if let Err(error) = self
                .repository
                .run(&["worktree", "add", &path_text, &branch])
            {
                // The branch was made just now, at `base`: take it back
                // unless something moved it meanwhile.
                let reference = format!("refs/heads/{branch}");
                self.repository
                    .run(&["update-ref", "-d", &reference, &base])
                    .ok();
                return Err(error.into());
            }
            branch
        };

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

    /// Remove the worktree of a step whose work is settled, keeping its
    /// branch: the branch holds the result; the worktree was only where the
    /// work happened.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn release(&self, workspace: Workspace) -> Result<(), WorkspaceError> {
        let _guard = lock(&self.lock)?;
        let path = workspace.path.display().to_string();
        self.repository
            .run(&["worktree", "remove", "--force", &path])?;
        self.repository.run(&["worktree", "prune"])?;
        Ok(())
    }

    /// Remove `node`'s worktree, if present. Its branches are kept: they
    /// hold the attempts' work, and expl never deletes a branch.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn discard(&self, node: NodeId) -> Result<(), WorkspaceError> {
        let _guard = lock(&self.lock)?;
        self.remove_worktree_locked(node)
    }

    fn remove_worktree_locked(&self, node: NodeId) -> Result<(), WorkspaceError> {
        let path = self.path_of(node);
        if path.exists() {
            let path_text = path.display().to_string();
            self.repository
                .run(&["worktree", "remove", "--force", &path_text])?;
        }
        self.repository.run(&["worktree", "prune"])?;
        Ok(())
    }

    /// Create a new branch for `node` at `base` under the first free
    /// [candidate](Self::branch_candidate) name, and return the name. A
    /// name is created only if it does not exist (`git branch` without
    /// `--force`); when git refuses because a ref is in the way, the next
    /// name is tried — unless the ref is at a parent path (`expl`), which
    /// every name shares. Names among `trunk` and their symbolic ref
    /// targets are reserved.
    fn allocate_branch_locked(
        &self,
        node: NodeId,
        base: &str,
        trunk: &[String],
    ) -> Result<String, WorkspaceError> {
        let mut reserved = HashSet::new();
        for name in trunk {
            let mut reference = if name.starts_with("refs/heads/") {
                name.clone()
            } else {
                format!("refs/heads/{name}")
            };
            // Even an intermediate symbolic ref can update the final
            // target when git creates a branch through it.
            while reserved.insert(reference.clone()) {
                let Some(target) = self.repository.symbolic_ref_target(&reference)? else {
                    break;
                };
                reference = target;
            }
        }

        let mut tried = 0;
        for attempt in 1..=BRANCH_ATTEMPTS {
            tried = attempt;
            let branch = Self::branch_candidate(node, attempt);
            if reserved.contains(&format!("refs/heads/{branch}")) {
                continue;
            }
            match self
                .repository
                .run(&["branch", "--no-track", &branch, base])
            {
                Ok(_) => return Ok(branch),
                Err(error @ GitError::Failed { .. }) => {
                    match self.repository.branch_blocker(&branch)? {
                        None => return Err(error.into()),
                        // Every candidate shares the parent path: none can
                        // be made.
                        Some(BranchBlocker::Parent(_)) => break,
                        Some(BranchBlocker::Exists | BranchBlocker::Nested(_)) => {}
                    }
                }
                Err(error @ GitError::Spawn(_)) => return Err(error.into()),
            }
        }
        Err(WorkspaceError::NoFreeBranch {
            node,
            first: Self::branch_name(node),
            tried,
        })
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

    /// The branch created for the step and checked out in it — the one to
    /// record on the step. Usually [`Workspaces::branch_name`], but a fresh
    /// name when a ref already took that one.
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

    /// Check that the worktree still has the step's own branch checked
    /// out: HEAD is attached to exactly `refs/heads/<branch>`. Every write
    /// expl makes in the worktree checks this first, since a write lands on
    /// whatever HEAD points at, and an agent may have switched the worktree
    /// to another branch (the trunk, say) or detached it.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::LeftBranch`] naming where HEAD points instead;
    /// [`WorkspaceError::Git`] when git fails.
    pub fn check_branch(&self) -> Result<(), WorkspaceError> {
        let head = Git::in_worktree(&self.path).head_ref()?;
        if head.as_deref() == Some(format!("refs/heads/{}", self.branch).as_str()) {
            return Ok(());
        }
        Err(WorkspaceError::LeftBranch {
            node: self.node,
            branch: self.branch.clone(),
            head,
        })
    }

    /// Put the worktree back at `head` on the step's branch, discarding
    /// what an attempt left behind (a reset and a clean, as
    /// [`Git::restore`] does) — only while the worktree is still on the
    /// step's branch ([`check_branch`](Self::check_branch)).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::LeftBranch`], with nothing reset, when the
    /// worktree left its branch; [`WorkspaceError::Git`] when git fails.
    pub fn restore(&self, head: &str) -> Result<(), WorkspaceError> {
        self.check_branch()?;
        Git::in_worktree(&self.path).restore(head)?;
        Ok(())
    }

    /// Conclude the predecessor merge whose [conflicts](Self::conflicts)
    /// were resolved in the worktree, then merge the remaining
    /// predecessors until the next conflict or until all are merged.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::LeftBranch`], with nothing written, when the
    /// worktree left the step's branch;
    /// [`WorkspaceError::UnresolvedConflicts`] when a conflicted file still
    /// holds conflict markers; [`WorkspaceError::MergeAbandoned`] when the
    /// merge was aborted instead of resolved; [`WorkspaceError::Git`] when
    /// git fails other than by a merge conflict.
    pub fn resume(&mut self) -> Result<(), WorkspaceError> {
        let git = Git::in_worktree(&self.path);
        // Even a resume with no pending merges requires a valid worktree.
        git.toplevel()?;
        self.check_branch()?;
        let Some(commit) = self.merging.clone() else {
            return self.merge_pending();
        };
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
        let git = Git::in_worktree(&self.path);
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
    /// conflict and leaving it in the worktree. Merges nothing when the
    /// worktree left the step's branch.
    fn merge_pending(&mut self) -> Result<(), WorkspaceError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.check_branch()?;
        let git = Git::in_worktree(&self.path);
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
    /// [`WorkspaceError::LeftBranch`], with nothing written, when the
    /// worktree left the step's branch; [`WorkspaceError::JoinIncomplete`]
    /// while predecessors are not all merged;
    /// [`WorkspaceError::UnresolvedConflicts`] when conflicts remain;
    /// [`WorkspaceError::Git`] when git fails.
    pub fn commit(&self, message: &str) -> Result<String, WorkspaceError> {
        if !self.is_joined() {
            return Err(WorkspaceError::JoinIncomplete(
                self.merging.iter().chain(&self.pending).cloned().collect(),
            ));
        }
        self.check_branch()?;
        let git = Git::in_worktree(&self.path);
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

    /// Put the worktree back at the commit it was created at, discarding
    /// what an attempt left behind (a reset and a clean, as
    /// [`Git::restore`] does) — only while HEAD is still detached, so no
    /// branch is ever reset.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::ScratchAttached`], with nothing reset, when a
    /// branch is checked out there; [`WorkspaceError::Git`] when git fails.
    pub fn restore(&self) -> Result<(), WorkspaceError> {
        let git = Git::in_worktree(&self.path);
        if let Some(head) = git.head_ref()? {
            return Err(WorkspaceError::ScratchAttached { head });
        }
        git.restore(&self.head)?;
        Ok(())
    }

    /// What changed since the worktree was created: whether HEAD moved,
    /// files changed, or untracked files appeared (ignored files aside).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn changes(&self) -> Result<ScratchChanges, WorkspaceError> {
        let git = Git::in_worktree(&self.path);
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::graph::Graph;

    fn git(dir: &Path, args: &[&str]) -> String {
        Git::new(dir).run(args).expect("git")
    }

    /// A repository on `main` with one commit, an initialised graph, and a
    /// planned step after the origin.
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        graph: Graph,
        workspaces: Workspaces,
        step: NodeId,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().join("repo");
            fs::create_dir(&root).expect("mkdir");
            git(&root, &["init", "-q", "-b", "main"]);
            git(&root, &["config", "user.name", "t"]);
            git(&root, &["config", "user.email", "t@example.com"]);
            fs::write(root.join("README"), "hi\n").expect("write");
            git(&root, &["add", "."]);
            git(&root, &["commit", "-q", "-m", "initial"]);
            let project = Project::at(&root);
            fs::create_dir_all(project.state_dir()).expect("state");
            let mut graph = Graph::open(&project.database_path()).expect("graph");
            let base = git(&root, &["rev-parse", "HEAD"]);
            let origin = graph
                .create_origin("Origin", Some("main"), &base)
                .expect("origin");
            let step = graph
                .propose_step("step", "step", &[origin], &[])
                .expect("step");
            Self {
                _dir: dir,
                root,
                graph,
                workspaces: Workspaces::new(&project),
                step,
            }
        }

        fn create(&self, trunk: &[String]) -> Result<Workspace, WorkspaceError> {
            let node = self.graph.node(self.step).expect("node");
            let origin = self.graph.origin().expect("origin");
            self.workspaces.create(&node, &[origin], trunk)
        }

        /// A branch `name` at a commit of its own, not checked out.
        fn occupy(&self, name: &str) -> String {
            git(&self.root, &["checkout", "-q", "-b", name]);
            fs::write(self.root.join("other.txt"), name).expect("write");
            git(&self.root, &["add", "."]);
            git(&self.root, &["commit", "-q", "-m", name]);
            git(&self.root, &["checkout", "-q", "main"]);
            self.tip(name)
        }

        fn tip(&self, branch: &str) -> String {
            git(&self.root, &["rev-parse", &format!("refs/heads/{branch}")])
        }

        fn has_branch(&self, branch: &str) -> bool {
            Git::new(&self.root).has_branch(branch).expect("branch")
        }
    }

    #[test]
    fn create_takes_the_first_name_when_it_is_free() {
        let fx = Fixture::new();
        let workspace = fx.create(&[]).expect("create");
        assert_eq!(workspace.branch(), Workspaces::branch_name(fx.step));
        assert_eq!(fx.tip(workspace.branch()), fx.tip("main"));
    }

    #[test]
    fn create_passes_over_taken_and_reserved_names_and_replaces_nothing() {
        let fx = Fixture::new();
        let id = fx.step;
        // Taken outright, by a branch nested under the name, and reserved
        // for a trunk that does not exist yet.
        let unrelated = fx.occupy(&Workspaces::branch_name(id));
        let nested_name = format!("{}/x", Workspaces::branch_candidate(id, 2));
        let nested = fx.occupy(&nested_name);
        let reserved = Workspaces::branch_candidate(id, 3);
        let trunk = vec!["main".to_owned(), format!("refs/heads/{reserved}")];

        let first = fx.create(&trunk).expect("create");
        let expected = Workspaces::branch_candidate(id, 4);
        assert_eq!(first.branch(), expected);
        assert_eq!(fx.tip(&expected), fx.tip("main"));
        assert_eq!(
            Git::new(first.path()).current_branch().expect("branch"),
            Some(expected.clone())
        );
        fs::write(first.path().join("work.txt"), "work").expect("write");
        let work = first.commit("work").expect("commit");

        // A retry neither reuses nor replaces the earlier attempt's branch.
        let retry = fx.create(&trunk).expect("retry");
        assert_eq!(retry.branch(), Workspaces::branch_candidate(id, 5));
        assert_eq!(fx.tip(&expected), work);
        assert_eq!(fx.tip(&Workspaces::branch_name(id)), unrelated);
        assert_eq!(fx.tip(&nested_name), nested);
        assert!(!fx.has_branch(&reserved), "a trunk name is never created");
    }

    #[test]
    fn create_leaves_a_symbolic_trunk_and_its_target_alone() {
        let fx = Fixture::new();
        let target = Workspaces::branch_name(fx.step);
        let target_ref = format!("refs/heads/{target}");
        let commit = fx.occupy(&target);
        git(&fx.root, &["symbolic-ref", "refs/heads/trunk", &target_ref]);

        let workspace = fx.create(&["trunk".to_owned()]).expect("create");
        assert_eq!(workspace.branch(), Workspaces::branch_candidate(fx.step, 2));
        assert_eq!(
            git(&fx.root, &["symbolic-ref", "refs/heads/trunk"]),
            target_ref
        );
        assert_eq!(fx.tip("trunk"), commit);
        assert_eq!(fx.tip(&target), commit);
    }

    #[test]
    fn create_leaves_a_dangling_symbolic_trunk_unresolved() {
        let fx = Fixture::new();
        let target = Workspaces::branch_name(fx.step);
        let target_ref = format!("refs/heads/{target}");
        let trunk = "refs/heads/trunk";
        git(&fx.root, &["symbolic-ref", trunk, &target_ref]);

        let workspace = fx.create(&[trunk.to_owned()]).expect("create");

        assert_eq!(workspace.branch(), Workspaces::branch_candidate(fx.step, 2));
        assert!(!fx.has_branch(&target), "the target is still uncreated");
        assert_eq!(git(&fx.root, &["symbolic-ref", trunk]), target_ref);
        assert!(Git::new(&fx.root).resolve_commit(trunk).is_err());
    }

    #[test]
    fn create_passes_over_every_ref_in_a_symbolic_trunk_chain() {
        let fx = Fixture::new();
        let first = format!("refs/heads/{}", Workspaces::branch_name(fx.step));
        let second = Workspaces::branch_candidate(fx.step, 2);
        let target_ref = format!("refs/heads/{second}");
        let trunk = "refs/heads/trunk";
        git(&fx.root, &["symbolic-ref", trunk, &first]);
        git(&fx.root, &["symbolic-ref", &first, &target_ref]);

        let workspace = fx.create(&["trunk".to_owned()]).expect("create");

        assert_eq!(workspace.branch(), Workspaces::branch_candidate(fx.step, 3));
        assert!(!fx.has_branch(&second), "the target is still uncreated");
        assert_eq!(
            git(&fx.root, &["symbolic-ref", "--no-recurse", trunk]),
            first
        );
        assert_eq!(git(&fx.root, &["symbolic-ref", trunk]), target_ref);
    }

    #[test]
    fn create_gives_up_when_no_name_is_free() {
        let fx = Fixture::new();
        let trunk: Vec<String> = (1..=BRANCH_ATTEMPTS)
            .map(|attempt| Workspaces::branch_candidate(fx.step, attempt))
            .collect();
        let error = fx.create(&trunk).expect_err("no free name");
        assert!(
            matches!(error, WorkspaceError::NoFreeBranch { node, tried, .. }
                if node == fx.step && tried == BRANCH_ATTEMPTS),
            "{error}"
        );
        assert!(!fx.workspaces.path_of(fx.step).exists());
    }

    #[test]
    fn create_gives_up_at_once_when_a_parent_path_is_a_branch() {
        let fx = Fixture::new();
        // A branch `expl` stands in the way of every `expl/…` name.
        let blocker = fx.occupy("expl");
        let error = fx.create(&[]).expect_err("no free name");
        assert!(
            matches!(error, WorkspaceError::NoFreeBranch { node, tried: 1, .. }
                if node == fx.step),
            "{error}"
        );
        assert_eq!(fx.tip("expl"), blocker);
        assert!(!fx.workspaces.path_of(fx.step).exists());
    }

    #[test]
    fn missing_worktree_git_file_cannot_redirect_operations_to_trunk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let repository = Git::new(root);
        let git = |args: &[&str]| repository.run(args).expect("git");
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.com"]);
        fs::write(root.join(".git/info/exclude"), "/.expl/\n").expect("exclude worktrees");
        fs::write(root.join("tracked"), "base\n").expect("tracked file");
        git(&["add", "tracked"]);
        git(&["commit", "-qm", "base"]);
        let base = git(&["rev-parse", "HEAD"]);

        let mut graph = Graph::in_memory().expect("graph");
        let origin = graph
            .create_origin("Origin", Some("main"), &base)
            .expect("origin");
        let step = graph
            .propose_step("Step", "Change code", &[origin], &[])
            .expect("step");
        let workspaces = Workspaces::new(&Project::at(root));
        let origin = graph.node(origin).expect("origin node");
        let mut workspace = workspaces
            .create(&graph.node(step).expect("step node"), &[origin], &[])
            .expect("step worktree");
        let worktree_git = Git::in_worktree(workspace.path());
        let scratch = workspaces
            .scratch_at(step, &base)
            .expect("scratch worktree");

        fs::write(root.join("tracked"), "trunk advanced\n").expect("advance trunk");
        git(&["commit", "-qam", "advance trunk"]);
        assert_ne!(git(&["rev-parse", "refs/heads/main"]), base);
        fs::write(root.join("tracked"), "staged\n").expect("stage change");
        git(&["add", "tracked"]);
        fs::write(root.join("tracked"), "unstaged\n").expect("unstaged change");
        fs::write(root.join("untracked"), "untracked\n").expect("untracked file");
        let checkout = || {
            [
                ".git/refs/heads/main",
                ".git/HEAD",
                ".git/index",
                "tracked",
                "untracked",
            ]
            .map(|path| fs::read(root.join(path)).expect(path))
        };
        let before = checkout();

        fs::remove_file(workspace.path().join(".git")).expect("remove step git file");
        let commit = workspace
            .commit("step commit")
            .expect_err("commit must fail");
        assert!(matches!(
            commit,
            WorkspaceError::Git(GitError::Failed { .. })
        ));
        assert_eq!(checkout(), before, "commit changed the main checkout");

        let resume = workspace.resume().expect_err("resume must fail");
        assert!(matches!(
            resume,
            WorkspaceError::Git(GitError::Failed { .. })
        ));
        assert_eq!(checkout(), before, "resume changed the main checkout");

        let restore = worktree_git.restore(&base).expect_err("restore must fail");
        assert!(matches!(restore, GitError::Failed { .. }));
        assert_eq!(checkout(), before, "restore changed the main checkout");

        fs::remove_file(scratch.path().join(".git")).expect("remove scratch git file");
        let changes = scratch.changes().expect_err("scratch inspection must fail");
        assert!(matches!(
            changes,
            WorkspaceError::Git(GitError::Failed { .. })
        ));
        assert_eq!(
            checkout(),
            before,
            "scratch inspection changed the main checkout"
        );
    }

    /// Where an agent took a step's worktree instead of the step's branch.
    #[derive(Debug, Clone, Copy)]
    enum Departure {
        Trunk,
        OtherBranch,
        Detached,
    }

    /// Every ref in the repository, with what it points at.
    fn refs(repository: &Git) -> String {
        repository
            .run(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .expect("refs")
    }

    #[test]
    fn a_worktree_that_left_its_branch_is_never_written_to() {
        for departure in [
            Departure::Trunk,
            Departure::OtherBranch,
            Departure::Detached,
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            let repository = Git::new(root);
            let git = |args: &[&str]| repository.run(args).expect("git");
            git(&["init", "-q", "-b", "main"]);
            git(&["config", "user.name", "test"]);
            git(&["config", "user.email", "test@example.com"]);
            fs::write(root.join(".git/info/exclude"), "/.expl/\n").expect("exclude worktrees");
            fs::write(root.join("tracked"), "base\n").expect("tracked file");
            git(&["add", "tracked"]);
            git(&["commit", "-qm", "base"]);
            let base = git(&["rev-parse", "HEAD"]);
            git(&["branch", "other"]);
            fs::write(root.join("tracked"), "second\n").expect("tracked file");
            git(&["commit", "-qam", "second"]);
            let second = git(&["rev-parse", "HEAD"]);

            let mut graph = Graph::in_memory().expect("graph");
            let origin = graph
                .create_origin("Origin", Some("main"), &base)
                .expect("origin");
            let step = graph
                .propose_step("Step", "Change code", &[origin], &[])
                .expect("step");
            let workspaces = Workspaces::new(&Project::at(root));
            let origin = graph.node(origin).expect("origin node");
            let mut workspace = workspaces
                .create(&graph.node(step).expect("step node"), &[origin], &[])
                .expect("step worktree");
            workspace.check_branch().expect("on its own branch");

            let worktree = Git::in_worktree(workspace.path());
            let expected_head = match departure {
                Departure::Trunk => {
                    worktree
                        .run(&["switch", "-q", "--ignore-other-worktrees", "main"])
                        .expect("switch to the trunk");
                    Some("refs/heads/main".to_owned())
                }
                Departure::OtherBranch => {
                    worktree
                        .run(&["switch", "-q", "other"])
                        .expect("switch to another branch");
                    Some("refs/heads/other".to_owned())
                }
                Departure::Detached => {
                    worktree.run(&["switch", "-q", "--detach"]).expect("detach");
                    None
                }
            };
            fs::write(workspace.path().join("tracked"), "agent edit\n").expect("edit");
            fs::write(workspace.path().join("new"), "agent file\n").expect("new file");

            let checkout = || {
                [".git/HEAD", ".git/index", "tracked"]
                    .map(|path| fs::read(root.join(path)).expect(path))
            };
            let refs_before = refs(&repository);
            let edited = workspace.path().to_path_buf();
            let checkout_before = checkout();
            let left = |error: WorkspaceError, what: &str| {
                let WorkspaceError::LeftBranch { node, branch, head } = error else {
                    panic!("{departure:?} {what}: {error}");
                };
                assert_eq!(node, step, "{departure:?} {what}");
                assert_eq!(
                    branch,
                    Workspaces::branch_name(step),
                    "{departure:?} {what}"
                );
                assert_eq!(head, expected_head, "{departure:?} {what}");
            };
            let unchanged = |what: &str| {
                assert_eq!(
                    refs(&repository),
                    refs_before,
                    "{departure:?}: {what} moved a ref"
                );
                assert_eq!(
                    checkout(),
                    checkout_before,
                    "{departure:?}: {what} changed the main checkout"
                );
                assert_eq!(
                    fs::read_to_string(edited.join("tracked")).expect("edit"),
                    "agent edit\n",
                    "{departure:?}: {what} reset the worktree"
                );
            };

            let error = workspace.commit("step commit").expect_err("commit refuses");
            assert!(
                error.to_string().contains(match departure {
                    Departure::Trunk => "`refs/heads/main`",
                    Departure::OtherBranch => "`refs/heads/other`",
                    Departure::Detached => "detached",
                }),
                "{error}"
            );
            left(error, "commit");
            unchanged("commit");
            left(workspace.resume().expect_err("resume refuses"), "resume");
            unchanged("resume");
            left(
                workspace.restore(&base).expect_err("restore refuses"),
                "restore",
            );
            unchanged("restore");
            workspace.pending.push_back(second.clone());
            left(
                workspace.merge_pending().expect_err("merge refuses"),
                "merge",
            );
            unchanged("merge");
            workspace.pending.clear();
            assert_eq!(git(&["rev-parse", "refs/heads/main"]), second);
            assert_eq!(git(&["rev-parse", "refs/heads/other"]), base);
            assert_eq!(
                git(&["rev-parse", &format!("refs/heads/{}", workspace.branch())]),
                base
            );
        }
    }

    #[test]
    fn a_worktree_on_its_branch_commits_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let repository = Git::new(root);
        let git = |args: &[&str]| repository.run(args).expect("git");
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.com"]);
        fs::write(root.join(".git/info/exclude"), "/.expl/\n").expect("exclude worktrees");
        fs::write(root.join("tracked"), "base\n").expect("tracked file");
        git(&["add", "tracked"]);
        git(&["commit", "-qm", "base"]);
        let base = git(&["rev-parse", "HEAD"]);

        let mut graph = Graph::in_memory().expect("graph");
        let origin = graph
            .create_origin("Origin", Some("main"), &base)
            .expect("origin");
        let step = graph
            .propose_step("Step", "Change code", &[origin], &[])
            .expect("step");
        let workspaces = Workspaces::new(&Project::at(root));
        let origin = graph.node(origin).expect("origin node");
        let workspace = workspaces
            .create(&graph.node(step).expect("step node"), &[origin], &[])
            .expect("step worktree");

        // A failed attempt's leftovers are restored away on the branch.
        fs::write(workspace.path().join("litter"), "x\n").expect("litter");
        workspace.restore(&base).expect("restore");
        assert!(!workspace.path().join("litter").exists());

        fs::write(workspace.path().join("tracked"), "step\n").expect("edit");
        let head = workspace.commit("step commit").expect("commit");
        assert_eq!(
            git(&["rev-parse", &format!("refs/heads/{}", workspace.branch())]),
            head
        );
        assert_eq!(git(&["rev-parse", "refs/heads/main"]), base);
        assert_eq!(git(&["show", &format!("{head}:tracked")]), "step");
    }

    #[test]
    fn a_scratch_worktree_with_a_branch_checked_out_is_not_restored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let repository = Git::new(root);
        let git = |args: &[&str]| repository.run(args).expect("git");
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.com"]);
        fs::write(root.join(".git/info/exclude"), "/.expl/\n").expect("exclude worktrees");
        fs::write(root.join("tracked"), "base\n").expect("tracked file");
        git(&["add", "tracked"]);
        git(&["commit", "-qm", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        fs::write(root.join("tracked"), "second\n").expect("tracked file");
        git(&["commit", "-qam", "second"]);
        let second = git(&["rev-parse", "HEAD"]);

        let workspaces = Workspaces::new(&Project::at(root));
        let scratch = workspaces.scratch_at(NodeId(1), &base).expect("scratch");
        fs::write(scratch.path().join("litter"), "x\n").expect("litter");
        scratch.restore().expect("restore a detached scratch");
        assert!(!scratch.path().join("litter").exists());

        Git::in_worktree(scratch.path())
            .run(&["switch", "-q", "--ignore-other-worktrees", "main"])
            .expect("switch to the trunk");
        let before = refs(&repository);
        let error = scratch.restore().expect_err("restore refuses");
        let WorkspaceError::ScratchAttached { head } = error else {
            panic!("{error}");
        };
        assert_eq!(head, "refs/heads/main");
        assert_eq!(refs(&repository), before);
        assert_eq!(git(&["rev-parse", "refs/heads/main"]), second);
    }
}
