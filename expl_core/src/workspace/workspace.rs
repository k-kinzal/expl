//! [`Workspaces`], [`Workspace`], and [`ScratchWorkspace`].

use std::collections::{HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use opentelemetry::KeyValue;

use crate::event::LeaseToken;
use crate::git::{BranchBlocker, Git, GitError};
use crate::graph::{Node, NodeId};
use crate::project::Project;
use crate::telemetry;
use crate::workspace::WorkspaceError;
use crate::workspace::clone::{AgentClone, Start, identity_args};

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

/// One verification attempt's own worktree, on its branch.
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

/// A private clone at a commit, detached, for one read-only reaction;
/// removed on drop.
#[derive(Debug)]
pub struct ScratchWorkspace {
    repository: Git,
    clone: AgentClone,
    /// The commit the clone was created at.
    head: String,
}

/// What was changed in a [`ScratchWorkspace`] since it was created; empty
/// when it is unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScratchChanges {
    /// The commit the clone was created at.
    pub created_at: String,
    /// Changed paths, sorted: files whose content differs from
    /// `created_at`, deleted files, and new files no ignore rule matches.
    pub paths: Vec<String>,
}

impl ScratchChanges {
    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

impl std::fmt::Display for ScratchChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.paths.is_empty() {
            f.write_str("no changes")
        } else {
            write!(f, "changed paths: {}", self.paths.join(", "))
        }
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

    /// Where the attempt holding `lease` verifies `node`. Each lease has
    /// its own path, including when a delivery's attempt count starts over.
    #[must_use]
    pub fn path_of(&self, node: NodeId, lease: &LeaseToken) -> PathBuf {
        self.root.join(format!("node-{}-a{lease}", node.0))
    }

    /// A fresh worktree for `node`, starting from its predecessors: the
    /// first one's tip, merged with the others in order. Merging stops at
    /// the first conflict, which is left in the worktree
    /// ([`Workspace::conflicts`]); once it is resolved,
    /// [`Workspace::resume`] merges the remaining predecessors.
    ///
    /// Only this attempt's own path ([`path_of`](Self::path_of) for `lease`)
    /// is allocated; worktrees of every other attempt of the step are left
    /// alone, because the process holding one may be the step's current
    /// holder. Each attempt releases its own worktree
    /// ([`release`](Self::release), [`unregister`](Self::unregister)), and
    /// `expl doctor` removes what is left once the step settles.
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
    /// taken; [`WorkspaceError::AttemptExists`] when this attempt already
    /// has a path or registration; [`WorkspaceError::Git`] when git fails
    /// other than by a name collision or a merge conflict;
    /// [`WorkspaceError::Io`] on filesystem failure.
    pub fn create(
        &self,
        node: &Node,
        lease: &LeaseToken,
        predecessors: &[Node],
        trunk: &[String],
    ) -> Result<Workspace, WorkspaceError> {
        telemetry::operation(
            "expl.workspace.allocate",
            vec![
                KeyValue::new("expl.node.id", node.id.0),
                KeyValue::new("expl.workspace.predecessors", predecessors.len() as i64),
            ],
            "workspace.allocate",
            || {
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
                let path = self.path_of(node.id, lease);
                let branch = {
                    let _guard = lock(&self.lock)?;
                    let registered = self.registered()?;
                    if path.try_exists()?
                        || registered
                            .iter()
                            .any(|(_, existing)| existing.file_name() == path.file_name())
                    {
                        return Err(WorkspaceError::AttemptExists(path));
                    }
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
                telemetry::attributes([
                    KeyValue::new("expl.workspace.branch", workspace.branch.clone()),
                    KeyValue::new(
                        "expl.workspace.conflict_count",
                        workspace.conflicts.len() as i64,
                    ),
                    KeyValue::new("expl.workspace.joined", workspace.is_joined()),
                ]);
                Ok(workspace)
            },
        )
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

    /// A private detached clone at `node`'s commit.
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

    /// A private detached clone at `commit`, for work about `node`.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails; [`WorkspaceError::Io`] on
    /// filesystem failure.
    pub fn scratch_at(
        &self,
        node: NodeId,
        commit: &str,
    ) -> Result<ScratchWorkspace, WorkspaceError> {
        telemetry::operation(
            "expl.workspace.scratch",
            vec![
                KeyValue::new("expl.node.id", node.0),
                KeyValue::new("expl.workspace.commit", commit.to_owned()),
            ],
            "workspace.scratch",
            || {
                let commit = self.repository.resolve_commit(commit)?;
                let clone = AgentClone::create(
                    &self.repository,
                    &self.root,
                    &format!("scratch-{}", node.0),
                    Start {
                        commit: commit.clone(),
                        branch: None,
                        merging: None,
                    },
                )?;
                Ok(ScratchWorkspace {
                    repository: self.repository.clone(),
                    clone,
                    head: commit,
                })
            },
        )
    }

    /// A private clone for the agent working on `workspace`'s step: at the
    /// worktree's HEAD, on a branch of the step's name, with the merge
    /// whose conflicts wait to be resolved there redone. The worktree
    /// itself stays expl's: what the agent produces is taken in from the
    /// clone ([`Workspace::take_in`], [`Workspace::take_resolution`]).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails; [`WorkspaceError::Io`] on
    /// filesystem failure.
    pub fn clone_for(&self, workspace: &Workspace) -> Result<AgentClone, WorkspaceError> {
        telemetry::operation(
            "expl.workspace.clone",
            vec![
                KeyValue::new("expl.node.id", workspace.node.0),
                KeyValue::new("expl.workspace.branch", workspace.branch.clone()),
                KeyValue::new("expl.workspace.joined", workspace.is_joined()),
            ],
            "workspace.clone",
            || {
                let commit = Git::in_worktree(&workspace.path).resolve_commit("HEAD")?;
                telemetry::attributes([KeyValue::new("expl.workspace.commit", commit.clone())]);
                AgentClone::create(
                    &self.repository,
                    &self.root,
                    &format!("node-{}", workspace.node.0),
                    Start {
                        commit,
                        branch: Some(workspace.branch.clone()),
                        merging: workspace.merging.clone(),
                    },
                )
            },
        )
    }

    /// The node worktrees registered with git, each with its node and path
    /// even if its directory is gone. Includes legacy worktree paths.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn registered(&self) -> Result<Vec<(NodeId, PathBuf)>, WorkspaceError> {
        Ok(self
            .repository
            .worktrees()?
            .into_iter()
            .filter_map(|path| self.node_at(&path).map(|node| (node, path)))
            .collect())
    }

    /// Node worktrees and directories left behind at their paths.
    pub(crate) fn node_paths(&self) -> Result<Vec<(NodeId, PathBuf)>, WorkspaceError> {
        let mut paths = self.registered()?;
        for (node, path) in self.node_directories()? {
            if !paths
                .iter()
                .any(|(_, existing)| existing.file_name() == path.file_name())
            {
                paths.push((node, path));
            }
        }
        Ok(paths)
    }

    /// Remove the node worktree at `path`, keeping its branch. Git removes
    /// a registered worktree when it is unlocked and either clean or gone.
    /// An unregistered directory is removed only when empty.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git refuses (the worktree has changes
    /// or is locked) or fails; [`WorkspaceError::Io`] when an unregistered
    /// directory is nonempty or cannot be removed.
    pub fn unregister(&self, path: &Path) -> Result<(), WorkspaceError> {
        let _guard = lock(&self.lock)?;
        if self.node_at(path).is_none() {
            return Ok(());
        }
        if let Some((_, registered)) = self
            .registered()?
            .into_iter()
            .find(|(_, candidate)| candidate.file_name() == path.file_name())
        {
            return self.remove_registered(&registered, false);
        }
        match std::fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
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

    /// Remove every worktree and leftover worktree directory for `node`.
    /// Its branches are kept, so any committed work stays available.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails; [`WorkspaceError::Io`] on
    /// filesystem failure.
    pub fn discard(&self, node: NodeId) -> Result<(), WorkspaceError> {
        let _guard = lock(&self.lock)?;
        for (_, path) in self.registered()?.into_iter().filter(|(id, _)| *id == node) {
            self.remove_registered(&path, true)?;
        }
        for (_, path) in self
            .node_directories()?
            .into_iter()
            .filter(|(id, _)| *id == node)
        {
            match std::fs::remove_dir_all(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// The managed node named by `path`, including pre-attempt names.
    fn node_at(&self, path: &Path) -> Option<NodeId> {
        if !same_dir(path.parent()?, &self.root) {
            return None;
        }
        let name = path.file_name()?.to_str()?.strip_prefix("node-")?;
        let id = if let Some((id, attempt)) = name.split_once("-a") {
            u128::from_str_radix(attempt, 16).ok()?;
            id
        } else {
            name
        };
        id.parse().ok().map(NodeId)
    }

    fn node_directories(&self) -> Result<Vec<(NodeId, PathBuf)>, WorkspaceError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut directories = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if let Some(node) = self.node_at(&path)
                && entry.file_type()?.is_dir()
            {
                directories.push((node, path));
            }
        }
        Ok(directories)
    }

    /// Remove a path recorded by git, holding the worktree lock.
    fn remove_registered(&self, path: &Path, force: bool) -> Result<(), WorkspaceError> {
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        let path = path.display().to_string();
        args.push(&path);
        self.repository.run(&args)?;
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

    /// Take in what the agent left in `clone` as the worktree's files: the
    /// clone's files as they are, whatever the agent committed or not on
    /// the way, with paths matching a pathspec in `frozen` left as they
    /// are at HEAD. Nothing is committed ([`commit`](Self::commit) does
    /// that).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::JoinIncomplete`] while predecessors are not all
    /// merged; [`WorkspaceError::LeftBranch`], with nothing written, when
    /// the worktree left the step's branch; [`WorkspaceError::Git`] when
    /// git fails.
    pub fn take_in(&self, clone: &AgentClone, frozen: &[String]) -> Result<(), WorkspaceError> {
        if !self.is_joined() {
            return Err(WorkspaceError::JoinIncomplete(
                self.merging.iter().chain(&self.pending).cloned().collect(),
            ));
        }
        self.check_branch()?;
        let git = Git::in_worktree(&self.path);
        let head = git.resolve_commit("HEAD")?;
        let tree = clone.snapshot(&head, frozen)?;
        git.run(&["read-tree", "-u", "--reset", &tree])?;
        Ok(())
    }

    /// Take in the agent's resolution of the merge in progress from
    /// `clone`: each conflicted file as the clone holds it (gone where the
    /// agent removed it). Everything else of the clone is left behind:
    /// the join concludes with what git merged plus the resolved files
    /// ([`resume`](Self::resume)).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::LeftBranch`], with nothing written, when the
    /// worktree left the step's branch; [`WorkspaceError::Git`] when git
    /// fails; [`WorkspaceError::Io`] on filesystem failure.
    pub fn take_resolution(&self, clone: &AgentClone) -> Result<(), WorkspaceError> {
        if self.merging.is_none() {
            return Ok(());
        }
        self.check_branch()?;
        let git = Git::in_worktree(&self.path);
        let head = git.resolve_commit("HEAD")?;
        let tree = clone.snapshot(&head, &[])?;
        for path in &self.conflicts {
            let object = format!("{tree}:{path}");
            if git.test(&["cat-file", "-e", &object])? {
                git.run(&["--literal-pathspecs", "checkout", &tree, "--", path])?;
            } else {
                match std::fs::remove_file(self.path.join(path)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }
}

impl ScratchWorkspace {
    /// The clone's directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.clone.path()
    }

    /// The clone's own directory, which holds the checkout: files handed
    /// to the agent alongside the repository go here, and are removed with
    /// the clone.
    #[must_use]
    pub fn directory(&self) -> &Path {
        self.clone.directory()
    }

    /// The commit the clone was created at.
    #[must_use]
    pub fn head(&self) -> &str {
        &self.head
    }

    /// Put the clone back as it was created, discarding whatever an
    /// attempt left behind.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails; [`WorkspaceError::Io`] on
    /// filesystem failure.
    pub fn restore(&self) -> Result<(), WorkspaceError> {
        self.clone.reset()
    }

    /// What changed in the clone's files since it was created: changed,
    /// deleted, and new files (ignored files aside). What the agent did
    /// with git there — commits, branches — is its own, and counts only as
    /// far as it changed the files.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn changes(&self) -> Result<ScratchChanges, WorkspaceError> {
        let tree = self.clone.snapshot(&self.head, &[])?;
        let paths = self.repository.diff_paths(&self.head, &tree)?;
        Ok(ScratchChanges {
            created_at: self.head.clone(),
            paths,
        })
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

        fn create(&self, attempt: u128, trunk: &[String]) -> Result<Workspace, WorkspaceError> {
            let node = self.graph.node(self.step).expect("node");
            let origin = self.graph.origin().expect("origin");
            self.workspaces
                .create(&node, &crate::event::test_token(attempt), &[origin], trunk)
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
        let workspace = fx.create(1, &[]).expect("create");
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

        let first = fx.create(1, &trunk).expect("create");
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
        let retry = fx.create(2, &trunk).expect("retry");
        assert_eq!(retry.branch(), Workspaces::branch_candidate(id, 5));
        assert_eq!(fx.tip(&expected), work);
        assert_eq!(fx.tip(&Workspaces::branch_name(id)), unrelated);
        assert_eq!(fx.tip(&nested_name), nested);
        assert!(!fx.has_branch(&reserved), "a trunk name is never created");
        first
            .check_branch()
            .expect("the earlier attempt's worktree is left alone");
    }

    #[test]
    fn create_leaves_a_symbolic_trunk_and_its_target_alone() {
        let fx = Fixture::new();
        let target = Workspaces::branch_name(fx.step);
        let target_ref = format!("refs/heads/{target}");
        let commit = fx.occupy(&target);
        git(&fx.root, &["symbolic-ref", "refs/heads/trunk", &target_ref]);

        let workspace = fx.create(1, &["trunk".to_owned()]).expect("create");
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

        let workspace = fx.create(1, &[trunk.to_owned()]).expect("create");

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

        let workspace = fx.create(1, &["trunk".to_owned()]).expect("create");

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
        let error = fx.create(1, &trunk).expect_err("no free name");
        assert!(
            matches!(error, WorkspaceError::NoFreeBranch { node, tried, .. }
                if node == fx.step && tried == BRANCH_ATTEMPTS),
            "{error}"
        );
        assert!(
            !fx.workspaces
                .path_of(fx.step, &crate::event::test_token(1))
                .exists()
        );
    }

    #[test]
    fn create_gives_up_at_once_when_a_parent_path_is_a_branch() {
        let fx = Fixture::new();
        // A branch `expl` stands in the way of every `expl/…` name.
        let blocker = fx.occupy("expl");
        let error = fx.create(1, &[]).expect_err("no free name");
        assert!(
            matches!(error, WorkspaceError::NoFreeBranch { node, tried: 1, .. }
                if node == fx.step),
            "{error}"
        );
        assert_eq!(fx.tip("expl"), blocker);
        assert!(
            !fx.workspaces
                .path_of(fx.step, &crate::event::test_token(1))
                .exists()
        );
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
            .create(
                &graph.node(step).expect("step node"),
                &crate::event::test_token(1),
                &[origin],
                &[],
            )
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

        // The scratch clone's files are read by the project's own git, so
        // inspecting them needs nothing of the clone's `.git`.
        fs::remove_dir_all(scratch.path().join(".git")).expect("remove scratch git directory");
        let changes = scratch.changes().expect("scratch inspection");
        assert!(changes.is_empty(), "{changes}");
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
                .create(
                    &graph.node(step).expect("step node"),
                    &crate::event::test_token(1),
                    &[origin],
                    &[],
                )
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
            .create(
                &graph.node(step).expect("step node"),
                &crate::event::test_token(1),
                &[origin],
                &[],
            )
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

    /// A repository with `tracked` at `base`, then `second` on `main`.
    fn two_commits(root: &Path) -> (Git, String, String) {
        let repository = Git::new(root);
        let git = |args: &[&str]| repository.run(args).expect("git");
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.com"]);
        fs::write(root.join(".git/info/exclude"), "/.expl/\n").expect("exclude worktrees");
        fs::write(root.join("tracked"), "base\n").expect("tracked file");
        git(&["add", "tracked"]);
        git(&["commit", "-qm", "base"]);
        git(&["tag", "v1"]);
        let base = git(&["rev-parse", "HEAD"]);
        fs::write(root.join("tracked"), "second\n").expect("tracked file");
        git(&["commit", "-qam", "second"]);
        let second = git(&["rev-parse", "HEAD"]);
        (repository, base, second)
    }

    #[test]
    fn a_scratch_clone_holds_the_history_and_keeps_the_project_refs_out_of_reach() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (repository, base, second) = two_commits(root);

        let workspaces = Workspaces::new(&Project::at(root));
        let scratch = workspaces.scratch_at(NodeId(1), &base).expect("scratch");
        let clone = Git::in_worktree(scratch.path());
        assert_eq!(clone.resolve_commit("HEAD").expect("head"), base);
        assert_eq!(clone.head_ref().expect("head ref"), None, "detached");
        assert_eq!(clone.resolve_commit("v1").expect("tag"), base);
        assert_eq!(
            clone.run(&["log", "--format=%s", &second]).expect("log"),
            "second\nbase",
            "the whole history is there"
        );

        // Whatever the agent does with git in its clone stays there.
        let before = refs(&repository);
        let force = |args: &[&str]| clone.run(args).expect("git in the clone");
        force(&["branch", "-f", "main", &second]);
        force(&["checkout", "-q", "main"]);
        fs::write(scratch.path().join("tracked"), "agent\n").expect("edit");
        force(&[
            "-c",
            "user.name=a",
            "-c",
            "user.email=a@a",
            "commit",
            "-qam",
            "agent",
        ]);
        assert_eq!(refs(&repository), before);
        assert_eq!(repository.resolve_commit("main").expect("main"), second);

        let changes = scratch.changes().expect("changes");
        assert_eq!(changes.paths, ["tracked"]);
        scratch.restore().expect("restore");
        assert!(scratch.changes().expect("changes").is_empty());
        assert_eq!(clone.head_ref().expect("head ref"), None);
        assert_eq!(clone.resolve_commit("HEAD").expect("head"), base);
        assert_eq!(refs(&repository), before);
    }

    #[test]
    fn a_step_takes_in_the_clones_files_with_the_frozen_paths_as_they_were() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let (repository, base, _) = two_commits(root);
        fs::write(root.join("README.md"), "readme\n").expect("readme");
        fs::create_dir(root.join("docs")).expect("docs");
        fs::write(root.join("docs/guide.md"), "guide\n").expect("guide");
        repository.run(&["add", "-A"]).expect("add");
        repository.run(&["commit", "-qm", "docs"]).expect("commit");

        let mut graph = Graph::in_memory().expect("graph");
        let head = repository.resolve_commit("HEAD").expect("head");
        let origin = graph
            .create_origin("Origin", Some("main"), &head)
            .expect("origin");
        let step = graph
            .propose_step("Step", "Change code", &[origin], &[])
            .expect("step");
        let workspaces = Workspaces::new(&Project::at(root));
        let origin = graph.node(origin).expect("origin node");
        let workspace = workspaces
            .create(
                &graph.node(step).expect("step node"),
                &crate::event::test_token(1),
                &[origin],
                &[],
            )
            .expect("step worktree");
        let clone = workspaces.clone_for(&workspace).expect("clone");
        let in_clone = Git::in_worktree(clone.path());
        assert_eq!(
            in_clone.current_branch().expect("branch").as_deref(),
            Some(workspace.branch())
        );

        // The agent commits part of its work itself, leaves the rest, and
        // touches the frozen Markdown both ways.
        fs::write(clone.path().join("tracked"), "agent\n").expect("edit");
        fs::write(clone.path().join("README.md"), "rewritten\n").expect("edit readme");
        in_clone
            .run(&[
                "-c",
                "user.name=a",
                "-c",
                "user.email=a@a",
                "commit",
                "-qam",
                "agent",
            ])
            .expect("agent's own commit");
        fs::remove_file(clone.path().join("docs/guide.md")).expect("remove guide");
        fs::write(clone.path().join("docs/new.md"), "new\n").expect("new doc");
        fs::write(clone.path().join("added"), "added\n").expect("new file");
        in_clone
            .run(&["branch", "-f", "main", &base])
            .expect("move the clone's main");

        let before = refs(&repository);
        workspace
            .take_in(&clone, &["*.md".to_owned()])
            .expect("take in");
        let tip = workspace.commit("step").expect("commit");
        let show = |path: &str| repository.run(&["show", &format!("{tip}:{path}")]);
        assert_eq!(show("tracked").expect("tracked"), "agent");
        assert_eq!(show("added").expect("added"), "added");
        assert_eq!(show("README.md").expect("readme"), "readme");
        assert_eq!(show("docs/guide.md").expect("guide"), "guide");
        assert!(show("docs/new.md").is_err(), "a new frozen file stays out");
        assert_eq!(
            repository
                .run(&["rev-list", "--count", &format!("{head}..{tip}")])
                .expect("count"),
            "1",
            "one commit of expl's on the step's start, the agent's own left behind"
        );
        let after = refs(&repository);
        let moved: Vec<&str> = after
            .lines()
            .filter(|line| !before.lines().any(|b| b == *line))
            .collect();
        assert_eq!(moved.len(), 1, "only the step's branch moved: {moved:?}");
        assert!(moved[0].starts_with(&format!("refs/heads/{} ", workspace.branch())));
    }
}
