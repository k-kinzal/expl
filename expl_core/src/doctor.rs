//! [`Doctor`] — check the graph against git.
//!
//! The graph records what happened; git holds the work. The two drift apart
//! when a human deletes or moves a step's branch, rewrites the trunk and
//! lets git collect its old commits, kills a process in the middle of an
//! execution, or leaves worktrees behind. [`Doctor::examine`] reports every
//! place where they disagree as an [`Inconsistency`], and
//! [`Doctor::repair`] fixes those that are safe to fix:
//!
//! | Inconsistency | Repair |
//! | --- | --- |
//! | a completed or failed step whose branch or head commit is missing, or whose branch tip is not its head commit | none: which side is right is the human's call |
//! | a running step whose `node.ready` nobody holds a lease on (an expired lease is left to the inbox, which delivers it again) | unclaimed: back to `planned`, with `node.ready` recorded again so execution is delivered it anew |
//! | an origin whose commit no longer exists | none: the trunk's history is gone from git |
//! | a worktree or leftover attempt directory for a step that is settled (completed, failed, pruned) or unknown | registration removed when its directory is gone; registered worktree removed when clean, keeping its branch; unregistered directory removed when empty |
//!
//! A failed step need not have committed anything (it can fail before its
//! worktree exists, or while joining its predecessors), so only what it
//! does record is checked; a completed change step must record both its branch
//! and its head commit.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

use crate::git::{Git, GitError};
use crate::graph::{Graph, GraphError, Node, NodeFilter, NodeId, NodeKind, NodeStatus};
use crate::project::Project;
use crate::workspace::{WorkspaceError, Workspaces};

/// Checking or repairing the graph against git failed.
#[derive(Debug, Error)]
pub enum DoctorError {
    /// The graph failed.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// git failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// A worktree could not be inspected or unregistered.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
}

/// One place where the graph and git disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Inconsistency {
    /// A settled step records no branch: a completed step, or a failed one
    /// that records a head commit.
    UnrecordedBranch {
        /// The step.
        node: NodeId,
        /// Completed or failed.
        status: NodeStatus,
    },
    /// A completed step records no head commit.
    UnrecordedCommit {
        /// The step.
        node: NodeId,
    },
    /// A settled step's branch does not exist in git.
    MissingBranch {
        /// The step.
        node: NodeId,
        /// Completed or failed.
        status: NodeStatus,
        /// The recorded branch.
        branch: String,
    },
    /// A settled step's head commit does not exist in git.
    MissingCommit {
        /// The step.
        node: NodeId,
        /// Completed or failed.
        status: NodeStatus,
        /// The recorded head commit.
        commit: String,
    },
    /// A settled step's branch tip is not its recorded head commit.
    BranchMoved {
        /// The step.
        node: NodeId,
        /// Completed or failed.
        status: NodeStatus,
        /// The recorded branch.
        branch: String,
        /// The recorded head commit.
        head_commit: String,
        /// The commit the branch points at.
        tip: String,
    },
    /// A running step whose `node.ready` nobody holds a lease on.
    OrphanedStep {
        /// The step.
        node: NodeId,
    },
    /// An origin whose commit no longer exists in git.
    MissingOriginCommit {
        /// The origin.
        node: NodeId,
        /// The recorded commit, if any.
        commit: Option<String>,
    },
    /// A worktree or leftover attempt directory for a step that is settled
    /// or unknown.
    StaleWorktree {
        /// The node the worktree is named for.
        node: NodeId,
        /// The step's status; `None` when no step has this id.
        status: Option<NodeStatus>,
        /// The worktree's path.
        path: PathBuf,
    },
}

impl Inconsistency {
    /// The node it concerns.
    #[must_use]
    pub fn node(&self) -> NodeId {
        match self {
            Self::UnrecordedBranch { node, .. }
            | Self::UnrecordedCommit { node }
            | Self::MissingBranch { node, .. }
            | Self::MissingCommit { node, .. }
            | Self::BranchMoved { node, .. }
            | Self::OrphanedStep { node }
            | Self::MissingOriginCommit { node, .. }
            | Self::StaleWorktree { node, .. } => *node,
        }
    }

    /// Short name of the kind, as serialised.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UnrecordedBranch { .. } => "unrecorded_branch",
            Self::UnrecordedCommit { .. } => "unrecorded_commit",
            Self::MissingBranch { .. } => "missing_branch",
            Self::MissingCommit { .. } => "missing_commit",
            Self::BranchMoved { .. } => "branch_moved",
            Self::OrphanedStep { .. } => "orphaned_step",
            Self::MissingOriginCommit { .. } => "missing_origin_commit",
            Self::StaleWorktree { .. } => "stale_worktree",
        }
    }

    /// Whether [`Doctor::repair`] can fix it.
    #[must_use]
    pub fn is_repairable(&self) -> bool {
        matches!(self, Self::OrphanedStep { .. } | Self::StaleWorktree { .. })
    }
}

impl fmt::Display for Inconsistency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnrecordedBranch { node, status } => {
                write!(f, "{status} step {node} records no branch")
            }
            Self::UnrecordedCommit { node } => {
                write!(f, "completed step {node} records no head commit")
            }
            Self::MissingBranch {
                node,
                status,
                branch,
            } => write!(f, "{status} step {node}: branch {branch} does not exist"),
            Self::MissingCommit {
                node,
                status,
                commit,
            } => write!(
                f,
                "{status} step {node}: head commit {commit} does not exist"
            ),
            Self::BranchMoved {
                node,
                status,
                branch,
                head_commit,
                tip,
            } => write!(
                f,
                "{status} step {node}: branch {branch} is at {tip}, not at its head commit \
                 {head_commit}"
            ),
            Self::OrphanedStep { node } => write!(
                f,
                "running step {node}: nobody holds a lease on its delivery"
            ),
            Self::MissingOriginCommit {
                node,
                commit: Some(commit),
            } => write!(f, "origin {node}: commit {commit} does not exist"),
            Self::MissingOriginCommit { node, commit: None } => {
                write!(f, "origin {node} records no commit")
            }
            Self::StaleWorktree {
                node,
                status: Some(status),
                path,
            } => write!(
                f,
                "worktree path {} was left by {status} step {node}",
                path.display()
            ),
            Self::StaleWorktree {
                node,
                status: None,
                path,
            } => write!(
                f,
                "worktree path {} belongs to unknown step {node}",
                path.display()
            ),
        }
    }
}

/// What [`Doctor::repair`] did about one inconsistency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repair {
    /// The inconsistency.
    pub inconsistency: Inconsistency,
    /// Whether it was fixed.
    pub repaired: bool,
    /// What was done, or why nothing was.
    pub note: String,
}

/// Checks one project's graph against its repository. See the
/// [module docs](self).
#[derive(Debug, Clone)]
pub struct Doctor {
    repository: Git,
    workspaces: Workspaces,
}

impl Doctor {
    /// The doctor of `project`.
    #[must_use]
    pub fn new(project: &Project) -> Self {
        Self {
            repository: Git::in_worktree(project.root()),
            workspaces: Workspaces::new(project),
        }
    }

    /// Every inconsistency between `graph` and git: settled steps first,
    /// then orphaned steps, origins, and stale worktrees, each by node.
    ///
    /// # Errors
    ///
    /// [`DoctorError`] when the graph or git cannot be read.
    pub fn examine(&self, graph: &Graph) -> Result<Vec<Inconsistency>, DoctorError> {
        let mut found = Vec::new();
        for step in graph.nodes(&NodeFilter {
            kinds: vec![NodeKind::Step],
            statuses: vec![NodeStatus::Completed, NodeStatus::Failed],
        })? {
            self.check_step(&step, &mut found)?;
        }
        for step in graph.orphans()? {
            found.push(Inconsistency::OrphanedStep { node: step.id });
        }
        for origin in graph.origins()? {
            let exists = match &origin.head_commit {
                Some(commit) => self.repository.has_commit(commit)?,
                None => false,
            };
            if !exists {
                found.push(Inconsistency::MissingOriginCommit {
                    node: origin.id,
                    commit: origin.head_commit,
                });
            }
        }
        let mut worktrees = self.workspaces.node_paths()?;
        worktrees.sort();
        for (node, path) in worktrees {
            let status = match owner(graph, node)? {
                Owner::Unsettled(_) => continue,
                Owner::Settled(status) => Some(status),
                Owner::Unknown => None,
            };
            found.push(Inconsistency::StaleWorktree { node, status, path });
        }
        Ok(found)
    }

    /// Repair what is safe to repair among `found` (as
    /// [examined](Self::examine)): unclaim orphaned steps so they are
    /// delivered again, and unregister stale worktrees. Each repair checks
    /// again that the inconsistency still holds, so one that resolved
    /// itself meanwhile is left alone. Inconsistencies that cannot be
    /// repaired are skipped; one repair per repairable inconsistency is
    /// returned, in order.
    ///
    /// # Errors
    ///
    /// [`DoctorError`] when the graph, git or the filesystem fails. A
    /// worktree that is locked or has changes, or a nonempty unregistered
    /// directory, is reported as not repaired.
    pub fn repair(
        &self,
        graph: &mut Graph,
        found: &[Inconsistency],
    ) -> Result<Vec<Repair>, DoctorError> {
        let mut repairs = Vec::new();
        for inconsistency in found {
            let (repaired, note) = match inconsistency {
                Inconsistency::OrphanedStep { node } => {
                    if graph.unclaim_orphan(*node)? {
                        (true, "unclaimed: planned again and redelivered".to_owned())
                    } else {
                        (
                            false,
                            "left alone: it settled or a live process took it up".to_owned(),
                        )
                    }
                }
                Inconsistency::StaleWorktree { node, path, .. } => {
                    self.unregister(graph, *node, path)?
                }
                Inconsistency::UnrecordedBranch { .. }
                | Inconsistency::UnrecordedCommit { .. }
                | Inconsistency::MissingBranch { .. }
                | Inconsistency::MissingCommit { .. }
                | Inconsistency::BranchMoved { .. }
                | Inconsistency::MissingOriginCommit { .. } => continue,
            };
            repairs.push(Repair {
                inconsistency: inconsistency.clone(),
                repaired,
                note,
            });
        }
        Ok(repairs)
    }

    /// Check what a completed or failed step records against git.
    fn check_step(&self, step: &Node, found: &mut Vec<Inconsistency>) -> Result<(), DoctorError> {
        let (node, status) = (step.id, step.status);
        let completed = status == NodeStatus::Completed;
        let tip = match &step.branch {
            // An investigation has no branch: its head commit is the code it
            // was about.
            None => {
                if !step.investigation && (completed || step.head_commit.is_some()) {
                    found.push(Inconsistency::UnrecordedBranch { node, status });
                }
                None
            }
            Some(branch) if !self.repository.has_branch(branch)? => {
                found.push(Inconsistency::MissingBranch {
                    node,
                    status,
                    branch: branch.clone(),
                });
                None
            }
            Some(branch) => Some((
                branch,
                self.repository
                    .resolve_commit(&format!("refs/heads/{branch}"))?,
            )),
        };
        match &step.head_commit {
            None if completed => found.push(Inconsistency::UnrecordedCommit { node }),
            None => {}
            Some(commit) if !self.repository.has_commit(commit)? => {
                found.push(Inconsistency::MissingCommit {
                    node,
                    status,
                    commit: commit.clone(),
                });
            }
            Some(commit) => {
                if let Some((branch, tip)) = tip
                    && self.repository.resolve_commit(commit)? != tip
                {
                    found.push(Inconsistency::BranchMoved {
                        node,
                        status,
                        branch: branch.clone(),
                        head_commit: commit.clone(),
                        tip,
                    });
                }
            }
        }
        Ok(())
    }

    /// Unregister the stale worktree of `node` at `path`, if it is still
    /// stale: whether it was, and what was done.
    fn unregister(
        &self,
        graph: &Graph,
        node: NodeId,
        path: &Path,
    ) -> Result<(bool, String), DoctorError> {
        if let Owner::Unsettled(status) = owner(graph, node)? {
            return Ok((false, format!("left alone: {node} is {status} now")));
        }
        let existed = path.exists();
        match self.workspaces.unregister(path) {
            Ok(()) => {}
            Err(WorkspaceError::Git(GitError::Failed { stderr, .. })) => {
                return Ok((false, format!("kept: {stderr}")));
            }
            Err(WorkspaceError::Io(error))
                if error.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
            {
                return Ok((false, "kept: directory contains files".to_owned()));
            }
            Err(error) => return Err(error.into()),
        }
        Ok(if existed {
            (
                true,
                "removed the worktree directory; any branch is kept".to_owned(),
            )
        } else {
            (true, "pruned the registration".to_owned())
        })
    }
}

/// Who a node worktree belongs to.
enum Owner {
    /// A step that may still need it, in this status.
    Unsettled(NodeStatus),
    /// A step that is settled, in this status: the worktree is stale.
    Settled(NodeStatus),
    /// No step has the worktree's id: the worktree is stale.
    Unknown,
}

/// Who the node worktree named for `node` belongs to.
fn owner(graph: &Graph, node: NodeId) -> Result<Owner, GraphError> {
    match graph.node(node) {
        Ok(step) if step.kind == NodeKind::Step => Ok(match step.status {
            NodeStatus::Completed | NodeStatus::Failed | NodeStatus::Pruned => {
                Owner::Settled(step.status)
            }
            NodeStatus::Proposed
            | NodeStatus::Planned
            | NodeStatus::Running
            | NodeStatus::Active
            | NodeStatus::Superseded => Owner::Unsettled(step.status),
        }),
        Ok(_) | Err(GraphError::NodeNotFound(_)) => Ok(Owner::Unknown),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Delivery, EventKind, Inbox, LeaseToken};
    use crate::graph::NewOrigin;
    use crate::process::{ProcessId, Processes};
    use crate::store::{self, EPOCH};

    fn git(dir: &Path, args: &[&str]) -> String {
        Git::new(dir).run(args).expect("git")
    }

    const READY: &[EventKind] = &[EventKind::NodeReady];

    /// A repository on `main` with one commit and an initialised graph.
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        project: Project,
        graph: Graph,
        doctor: Doctor,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().join("repo");
            std::fs::create_dir(&root).expect("mkdir");
            git(&root, &["init", "-q", "-b", "main"]);
            git(&root, &["config", "user.name", "t"]);
            git(&root, &["config", "user.email", "t@example.com"]);
            std::fs::write(root.join("README"), "hi\n").expect("write");
            git(&root, &["add", "."]);
            git(&root, &["commit", "-q", "-m", "initial"]);
            let project = Project::at(&root);
            std::fs::create_dir_all(project.state_dir()).expect("state");
            let mut graph = Graph::open(&project.database_path()).expect("graph");
            let base = git(&root, &["rev-parse", "HEAD"]);
            graph
                .create_origin("Origin", Some("main"), &base)
                .expect("origin");
            let doctor = Doctor::new(&project);
            Self {
                _dir: dir,
                root,
                project,
                graph,
                doctor,
            }
        }

        fn origin(&self) -> NodeId {
            self.graph.origin().expect("origin").id
        }

        fn workspaces(&self) -> Workspaces {
            Workspaces::new(&self.project)
        }

        /// A step after the origin, planned (records `node.ready`).
        fn planned(&mut self) -> NodeId {
            let origin = self.origin();
            let id = self
                .graph
                .propose_step("step", "step", &[origin], &[])
                .expect("step");
            self.graph.plan_step(id, 1).expect("plan");
            id
        }

        /// Lease and claim a planned step's `node.ready`.
        fn claim(&mut self, id: NodeId) -> Delivery {
            let process = self.process("executor");
            let mut inbox = self.inbox();
            loop {
                let delivery = inbox
                    .lease(READY, process, false, crate::process::MAX_ATTEMPTS)
                    .expect("lease")
                    .expect("the step's node.ready");
                if delivery.event.node == Some(id) {
                    assert!(self.graph.claim(id, &delivery.token).expect("claim"));
                    return delivery;
                }
                inbox.ack(&delivery, None).expect("ack");
            }
        }

        /// A planned step whose `node.ready` a live process leased and
        /// which it claimed and gave a worktree, as execution does.
        fn executing(&mut self) -> NodeId {
            let id = self.planned();
            let delivery = self.claim(id);
            let node = self.graph.node(id).expect("node");
            let origin = self.graph.origin().expect("origin");
            self.workspaces()
                .create(&node, &delivery.token, &[origin], &[])
                .expect("create");
            id
        }

        /// A step executed as execution does: in its own worktree, which
        /// commits `file` and is left in place; then `complete`d or failed.
        fn executed(&mut self, file: &str, complete: bool) -> (NodeId, PathBuf) {
            let id = self.planned();
            let delivery = self.claim(id);
            let node = self.graph.node(id).expect("node");
            let origin = self.graph.origin().expect("origin");
            let workspace = self
                .workspaces()
                .create(&node, &delivery.token, &[origin], &[])
                .expect("create");
            std::fs::write(workspace.path().join(file), file).expect("write");
            let head = workspace.commit(file).expect("commit");
            self.graph
                .set_branch(id, &delivery.token, workspace.branch(), Some(&head))
                .expect("branch");
            if complete {
                self.graph
                    .complete(id, &delivery.token, "done")
                    .expect("complete");
            } else {
                self.graph
                    .fail(id, &delivery.token, "broken")
                    .expect("fail");
            }
            self.inbox().ack(&delivery, None).expect("ack");
            (id, workspace.path().to_owned())
        }

        /// Like [`executed`](Self::executed), without leaving the worktree.
        fn settled(&mut self, file: &str, complete: bool) -> NodeId {
            let (id, path) = self.executed(file, complete);
            let path = path.display().to_string();
            git(&self.root, &["worktree", "remove", "--force", &path]);
            id
        }

        fn examine(&self) -> Vec<Inconsistency> {
            self.doctor.examine(&self.graph).expect("examine")
        }

        fn repair(&mut self, found: &[Inconsistency]) -> Vec<Repair> {
            self.doctor.repair(&mut self.graph, found).expect("repair")
        }

        /// A running process of `agent`, beating now.
        fn process(&self, agent: &str) -> ProcessId {
            let mut processes =
                Processes::open(&self.project.database_path(), &self.project.processes_dir())
                    .expect("processes");
            let record = processes.create(agent, None, true).expect("create");
            assert!(
                processes
                    .start(record.id, std::process::id())
                    .expect("start")
            );
            record.id
        }

        /// Stop `process` beating long ago, as if it was killed.
        fn kill(&self, process: ProcessId) {
            store::open(&self.project.database_path())
                .expect("db")
                .execute(
                    &format!("UPDATE processes SET heartbeat_at = {EPOCH} - 3600 WHERE id = ?1"),
                    [process.0],
                )
                .expect("kill");
        }

        fn inbox(&self) -> Inbox {
            Inbox::open(&self.project.database_path(), "executor").expect("inbox")
        }
    }

    #[test]
    fn a_consistent_graph_has_nothing_to_report() {
        let mut fx = Fixture::new();
        fx.settled("a.txt", true);
        fx.settled("b.txt", false);
        let origin = fx.origin();
        let never_committed = fx
            .graph
            .propose_step("c", "c", &[origin], &[])
            .expect("step");
        fx.graph.plan_step(never_committed, 1).expect("plan");
        let delivery = fx.claim(never_committed);
        fx.graph
            .fail(never_committed, &delivery.token, "no worktree")
            .expect("fail");
        fx.executing();
        let origin = fx.graph.origin().expect("origin");
        let _scratch = fx.workspaces().scratch(&origin).expect("scratch");

        assert_eq!(fx.examine(), Vec::new());
    }

    #[test]
    fn steps_whose_branch_or_commit_drifted_are_detected_and_left_alone() {
        let mut fx = Fixture::new();
        let deleted = fx.settled("deleted.txt", true);
        git(
            &fx.root,
            &["branch", "-D", &Workspaces::branch_name(deleted)],
        );

        let moved = fx.settled("moved.txt", false);
        let moved_branch = Workspaces::branch_name(moved);
        let recorded = fx
            .graph
            .node(moved)
            .expect("node")
            .head_commit
            .expect("head");
        git(&fx.root, &["branch", "-f", &moved_branch, "main"]);
        let tip = git(&fx.root, &["rev-parse", "main"]);

        let gone = fx.settled("gone.txt", true);
        let fabricated = "0123456789abcdef0123456789abcdef01234567";
        store::open(&fx.project.database_path())
            .expect("db")
            .execute(
                "UPDATE nodes SET head_commit = ?1 WHERE id = ?2",
                rusqlite::params![fabricated, gone.0],
            )
            .expect("record a missing commit");

        let unrecorded = fx.planned();
        let delivery = fx.claim(unrecorded);
        fx.graph
            .complete(unrecorded, &delivery.token, "done")
            .expect("complete");

        let found = fx.examine();
        assert_eq!(
            found,
            vec![
                Inconsistency::MissingBranch {
                    node: deleted,
                    status: NodeStatus::Completed,
                    branch: Workspaces::branch_name(deleted),
                },
                Inconsistency::BranchMoved {
                    node: moved,
                    status: NodeStatus::Failed,
                    branch: moved_branch,
                    head_commit: recorded,
                    tip,
                },
                Inconsistency::MissingCommit {
                    node: gone,
                    status: NodeStatus::Completed,
                    commit: fabricated.to_owned(),
                },
                Inconsistency::UnrecordedBranch {
                    node: unrecorded,
                    status: NodeStatus::Completed,
                },
                Inconsistency::UnrecordedCommit { node: unrecorded },
            ]
        );
        assert!(found.iter().all(|i| !i.is_repairable()));
        assert_eq!(fx.repair(&found), Vec::new());
        assert_eq!(fx.examine(), found);
    }

    #[test]
    fn orphaned_running_steps_are_detected_and_unclaimed_for_redelivery() {
        let mut fx = Fixture::new();
        let [held, killed, given_up, unleased] = [(); 4].map(|()| fx.planned());

        // Each of the first three is leased, by its own process, and
        // claimed; the last was claimed before holder tokens were recorded.
        let mut leases = Vec::new();
        for id in [held, killed, given_up] {
            let process = fx.process("executor");
            let delivery = fx
                .inbox()
                .lease(READY, process, false, crate::process::MAX_ATTEMPTS)
                .expect("lease")
                .expect("ready");
            assert_eq!(delivery.event.node, Some(id));
            assert!(fx.graph.claim(id, &delivery.token).expect("claim"));
            leases.push((process, delivery));
        }
        store::open(&fx.project.database_path())
            .expect("db")
            .execute(
                "UPDATE nodes SET status = 'running' WHERE id = ?1",
                [unleased.0],
            )
            .expect("legacy running step");
        fx.kill(leases[1].0);
        assert!(
            !fx.inbox().fail(&leases[2].1, "boom", 1).expect("give up"),
            "the delivery is given up, so it is never redelivered"
        );

        // The killed holder's step is not orphaned: its lease is still the
        // inbox's to deliver again once it expires. Unclaiming it too would
        // hand the step out twice — and a holder that merely looks dead may
        // only be slow.
        let found = fx.examine();
        let orphans = [given_up, unleased]
            .map(|node| Inconsistency::OrphanedStep { node })
            .to_vec();
        assert_eq!(found, orphans);

        let repairs = fx.repair(&found);
        assert_eq!(repairs.len(), 2);
        assert!(repairs.iter().all(|r| r.repaired), "{repairs:?}");
        for id in [held, killed] {
            assert_eq!(fx.graph.node(id).expect("step").status, NodeStatus::Running);
        }
        for id in [given_up, unleased] {
            assert_eq!(fx.graph.node(id).expect("step").status, NodeStatus::Planned);
        }
        assert_eq!(fx.examine(), Vec::new());

        // Redelivered: a new process is handed the given-up and the
        // unleased step again. The killed one's events wait for its dead
        // holder's lease to expire.
        let fresh = fx.process("executor");
        let mut inbox = fx.inbox();
        let mut delivered = Vec::new();
        while let Some(delivery) = inbox
            .lease(READY, fresh, false, crate::process::MAX_ATTEMPTS)
            .expect("lease")
        {
            delivered.push(delivery.event.node.expect("about a step"));
        }
        delivered.sort();
        assert_eq!(delivered, vec![given_up, unleased]);

        // A repair that comes too late changes nothing.
        let late = fx.repair(&[Inconsistency::OrphanedStep { node: held }]);
        assert!(!late[0].repaired);
        assert_eq!(
            fx.graph.node(held).expect("held").status,
            NodeStatus::Running
        );
    }

    #[test]
    fn origins_whose_commit_is_gone_are_detected() {
        let mut fx = Fixture::new();
        let previous = fx.origin();
        let tree = git(&fx.root, &["rev-parse", "HEAD^{tree}"]);
        let dangling = git(&fx.root, &["commit-tree", "-m", "rewritten", &tree]);
        let rewritten = fx
            .graph
            .advance_origin(&NewOrigin {
                previous,
                title: "rewritten".into(),
                note: String::new(),
                branch: Some("main".into()),
                commit: dangling.clone(),
                adopted: Vec::new(),
            })
            .expect("advance")
            .expect("moved");
        assert_eq!(fx.examine(), Vec::new());

        git(&fx.root, &["prune", "--expire=now"]);
        let found = fx.examine();
        assert_eq!(
            found,
            vec![Inconsistency::MissingOriginCommit {
                node: rewritten,
                commit: Some(dangling),
            }]
        );
        assert_eq!(fx.repair(&found), Vec::new());
    }

    #[test]
    fn stale_worktrees_are_detected_and_pruned() {
        let mut fx = Fixture::new();
        let (completed, completed_path) = fx.executed("done.txt", true);
        let (dirty, dirty_path) = fx.executed("broken.txt", false);
        let workspaces = fx.workspaces();
        std::fs::write(dirty_path.join("wip.txt"), "wip").expect("write");
        let (pruned, _) = fx.executed("pruned.txt", false);
        fx.graph.prune(pruned, "by operator").expect("prune");

        let unknown = NodeId(999);
        let unknown_path = fx
            .project
            .worktrees_dir()
            .join(format!("node-{}", unknown.0));
        git(
            &fx.root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &Workspaces::branch_name(unknown),
                &unknown_path.display().to_string(),
                "main",
            ],
        );
        let running = fx.executing();
        std::fs::remove_dir_all(&unknown_path).expect("remove");
        let empty_attempt = workspaces.path_of(completed, &LeaseToken::for_test(2));
        std::fs::create_dir(&empty_attempt).expect("empty leftover");
        let dirty_attempt = workspaces.path_of(dirty, &LeaseToken::for_test(2));
        std::fs::create_dir(&dirty_attempt).expect("leftover");
        std::fs::write(dirty_attempt.join("wip.txt"), "wip").expect("write");

        let found = fx.examine();
        let stale: Vec<(NodeId, Option<NodeStatus>)> = found
            .iter()
            .map(|i| {
                let Inconsistency::StaleWorktree { node, status, .. } = i else {
                    panic!("unexpected {i}");
                };
                (*node, *status)
            })
            .collect();
        assert_eq!(
            stale,
            vec![
                (completed, Some(NodeStatus::Completed)),
                (completed, Some(NodeStatus::Completed)),
                (dirty, Some(NodeStatus::Failed)),
                (dirty, Some(NodeStatus::Failed)),
                (pruned, Some(NodeStatus::Pruned)),
                (unknown, None),
            ]
        );
        assert!(
            !found.iter().any(|i| i.node() == running),
            "a running step keeps its worktree"
        );

        let repairs = fx.repair(&found);
        let outcome: Vec<(NodeId, bool)> = repairs
            .iter()
            .map(|r| (r.inconsistency.node(), r.repaired))
            .collect();
        assert_eq!(
            outcome,
            vec![
                (completed, true),
                (completed, true),
                (dirty, false),
                (dirty, false),
                (pruned, true),
                (unknown, true)
            ]
        );
        assert!(!completed_path.exists());
        assert!(!empty_attempt.exists());
        for path in [dirty_path, dirty_attempt] {
            assert!(path.join("wip.txt").exists(), "uncommitted work is kept");
        }
        let branches = Git::new(&fx.root);
        for id in [completed, pruned, unknown] {
            assert!(
                branches
                    .has_branch(&Workspaces::branch_name(id))
                    .expect("branch"),
                "the branch of {id} is kept"
            );
        }

        let remaining = fx.examine();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|i| i.node() == dirty));
    }
}
