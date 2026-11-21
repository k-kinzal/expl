//! [`Workspaces`] and [`Workspace`].

use std::path::{Path, PathBuf};

use crate::git::{Git, GitError};
use crate::graph::{Node, NodeId};
use crate::project::Project;
use crate::workspace::WorkspaceError;

/// The node worktrees of one project. See the [module docs](crate::workspace).
#[derive(Debug, Clone)]
pub struct Workspaces {
    repository: Git,
    root: PathBuf,
}

/// One node's worktree.
#[derive(Debug, Clone)]
pub struct Workspace {
    node: NodeId,
    path: PathBuf,
    branch: String,
    /// Files left conflicted by joining predecessor branches.
    conflicts: Vec<String>,
}

impl Workspaces {
    /// The worktrees of `project`, under `.expl/worktrees/`.
    #[must_use]
    pub fn new(project: &Project) -> Self {
        Self {
            repository: Git::new(project.root()),
            root: project.state_dir().join("worktrees"),
        }
    }

    /// The branch that holds `node`.
    #[must_use]
    pub fn branch_name(node: NodeId) -> String {
        format!("expl/node-{}", node.0)
    }

    /// Where `node`'s worktree lives.
    #[must_use]
    pub fn path_of(&self, node: NodeId) -> PathBuf {
        self.root.join(format!("node-{}", node.0))
    }

    /// The existing worktree of a realised node (origin or completed step),
    /// re-creating it from the node's recorded commit if it was removed.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NoCommit`] when the node was never realised.
    pub fn open(&self, node: &Node) -> Result<Workspace, WorkspaceError> {
        let commit = node
            .head_commit
            .as_deref()
            .ok_or(WorkspaceError::NoCommit(node.id))?;
        let path = self.path_of(node.id);
        let branch = Self::branch_name(node.id);
        if !path.join(".git").exists() {
            std::fs::create_dir_all(&self.root)?;
            let path_text = path.display().to_string();
            if self.branch_exists(&branch)? {
                self.repository
                    .run_exclusive(&["worktree", "add", &path_text, &branch])?;
            } else {
                self.repository
                    .run_exclusive(&["worktree", "add", "-b", &branch, &path_text, commit])?;
            }
        }
        Ok(Workspace {
            node: node.id,
            path,
            branch,
            conflicts: Vec::new(),
        })
    }

    /// A fresh worktree for `node`, starting from its predecessors. Any
    /// previous worktree and branch of the node (a failed attempt) are
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

        self.discard(node.id)?;
        std::fs::create_dir_all(&self.root)?;
        let path = self.path_of(node.id);
        let branch = Self::branch_name(node.id);
        let path_text = path.display().to_string();
        self.repository
            .run_exclusive(&["worktree", "add", "-b", &branch, &path_text, base])?;

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
                    // Leave the conflict for the executor; further joins
                    // cannot start until this merge is concluded.
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
            conflicts,
        })
    }

    /// Remove `node`'s worktree and branch, if present.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn discard(&self, node: NodeId) -> Result<(), WorkspaceError> {
        let path = self.path_of(node);
        if path.exists() {
            let path_text = path.display().to_string();
            self.repository
                .run_exclusive(&["worktree", "remove", "--force", &path_text])?;
        }
        self.repository.run_exclusive(&["worktree", "prune"])?;
        let branch = Self::branch_name(node);
        if self.branch_exists(&branch)? {
            self.repository.run_exclusive(&["branch", "-D", &branch])?;
        }
        Ok(())
    }

    fn branch_exists(&self, branch: &str) -> Result<bool, WorkspaceError> {
        let reference = format!("refs/heads/{branch}");
        match self
            .repository
            .run(&["show-ref", "--verify", "--quiet", &reference])
        {
            Ok(_) => Ok(true),
            Err(GitError::Failed { .. }) => Ok(false),
            Err(error @ GitError::Spawn(_)) => Err(error.into()),
        }
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
            git.run_exclusive(&args)?;
        }
        Ok(git.run(&["rev-parse", "HEAD"])?)
    }

    /// Reset the worktree to `commit`, dropping uncommitted changes and
    /// untracked (non-ignored) files.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn restore(&self, commit: &str) -> Result<(), WorkspaceError> {
        let git = Git::new(&self.path);
        if git
            .run(&["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .is_ok()
        {
            git.run(&["merge", "--abort"])?;
        }
        git.run(&["reset", "--hard", commit])?;
        git.run(&["clean", "-fd"])?;
        Ok(())
    }

    /// `git diff --stat base..HEAD`, for prompts.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Git`] when git fails.
    pub fn diff_stat(&self, base: &str) -> Result<String, WorkspaceError> {
        let range = format!("{base}..HEAD");
        Ok(Git::new(&self.path).run(&["diff", "--stat", &range])?)
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
