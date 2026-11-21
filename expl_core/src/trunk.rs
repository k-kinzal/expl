//! [`Trunk`] — the default branch expl follows.
//!
//! Exploration starts from the **origin**: the trunk as it stands. Agents
//! never touch the trunk. The owner moves it with git, merging the
//! candidates they choose and resolving conflicts as with any branch.
//! expl only follows: when the trunk's tip moves, [`Trunk::follow`] records
//! the new commit as the active origin and keeps the previous one,
//! superseded, so the graph remembers what was taken in and when
//! ([`Lineage`](crate::graph::Lineage)).
//!
//! Git is the ground truth for what was taken in. A completed step is
//! adopted when the new trunk contains its work and the previous one did
//! not: by history (a merge or fast-forward), or by patch (`git cherry`:
//! squash merges and cherry-picks of the step's commits).
//!
//! Every running [process](crate::process::Process) follows the trunk, so
//! it is enough to merge: the origin moves within seconds.

use std::collections::BTreeSet;

use serde::Serialize;
use thiserror::Error;

use crate::git::{Git, GitError};
use crate::graph::{Graph, GraphError, NewOrigin, NodeFilter, NodeId, NodeKind, NodeStatus};
use crate::project::Project;

/// Following the trunk failed.
#[derive(Debug, Error)]
pub enum TrunkError {
    /// The graph refused.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// git failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// The trunk branch does not exist.
    #[error("trunk branch `{0}` does not exist")]
    MissingBranch(String),
    /// git could not tell whether `trunk` holds `commit` (for instance, a
    /// commit no longer exists), so what the trunk took in is unknown.
    #[error("cannot tell whether {trunk} holds {commit}: {source}")]
    Ancestry {
        /// The commit that may hold the other.
        trunk: String,
        /// The commit that may be held.
        commit: String,
        /// What git reported.
        #[source]
        source: GitError,
    },
}

/// What one move of the trunk recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Advance {
    /// The new, active origin.
    pub origin: NodeId,
    /// The origin it superseded.
    pub previous: NodeId,
    /// The trunk's commit.
    pub commit: String,
    /// The trunk branch.
    pub branch: String,
    /// Steps adopted into the new origin (the newest of each line of work;
    /// the steps they built on come with them).
    pub adopted: Vec<NodeId>,
    /// Whether the new commit contains the previous origin's. `false` when
    /// the trunk was rewritten rather than moved forward.
    pub descends: bool,
}

/// The project's trunk. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct Trunk {
    git: Git,
    branch: Option<String>,
}

impl Trunk {
    /// The trunk of `project`: `branch` as declared (`trunk` in
    /// `expl.toml`), else the branch the current origin was read from.
    #[must_use]
    pub fn new(project: &Project, branch: Option<&str>) -> Self {
        Self {
            git: Git::new(project.root()),
            branch: branch.filter(|b| !b.is_empty()).map(str::to_owned),
        }
    }

    /// If the trunk moved since the current origin, record its tip as the
    /// active origin, adopting the completed steps it took in. Records
    /// `origin.advanced`. `None` when there is no trunk to follow, or it
    /// has not moved (or another process recorded the move first).
    ///
    /// # Errors
    ///
    /// [`TrunkError::MissingBranch`] when the trunk branch does not exist;
    /// [`TrunkError::Ancestry`] when git cannot tell what the trunk holds
    /// (nothing is recorded then); [`TrunkError::Graph`] on graph failure.
    pub fn follow(&self, graph: &mut Graph) -> Result<Option<Advance>, TrunkError> {
        let current = graph.origin()?;
        let Some(branch) = self
            .branch
            .clone()
            .or_else(|| current.branch.clone())
            .filter(|b| !b.is_empty())
        else {
            return Ok(None);
        };
        let commit = self
            .git
            .resolve_commit(&format!("refs/heads/{branch}"))
            .map_err(|_| TrunkError::MissingBranch(branch.clone()))?;
        let previous = current.head_commit.clone().unwrap_or_default();
        if commit == previous {
            return Ok(None);
        }
        let descends = previous.is_empty() || self.contains(&commit, &previous)?;

        let lineage = graph.lineage()?;
        let mut adopted: BTreeSet<NodeId> = BTreeSet::new();
        for step in graph.nodes(&NodeFilter {
            kinds: vec![NodeKind::Step],
            statuses: vec![NodeStatus::Completed],
        })? {
            let Some(head) = step.head_commit.as_deref() else {
                continue;
            };
            if lineage.in_trunk(step.id) {
                continue;
            }
            let already = !previous.is_empty() && self.takes_in(&previous, head)?;
            if !already && self.takes_in(&commit, head)? {
                adopted.insert(step.id);
            }
        }

        // Keep the newest of each line of work: the rest come with it.
        let mut newest = Vec::new();
        for &id in &adopted {
            let mut covered = false;
            for &other in &adopted {
                if other != id && graph.ancestors(other)?.iter().any(|n| n.id == id) {
                    covered = true;
                    break;
                }
            }
            if !covered {
                newest.push(id);
            }
        }

        let title = self
            .git
            .run(&["log", "-1", "--format=%s", &commit])
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Origin".to_owned());
        let recorded = graph.advance_origin(&NewOrigin {
            title,
            note: String::new(),
            branch: Some(branch.clone()),
            commit: commit.clone(),
            adopted: newest.clone(),
        })?;
        Ok(recorded.map(|origin| Advance {
            origin,
            previous: current.id,
            commit,
            branch,
            adopted: newest,
            descends,
        }))
    }

    /// Whether `ancestor` is in `commit`'s history.
    ///
    /// # Errors
    ///
    /// [`TrunkError::Ancestry`] when git fails, e.g. on an unknown commit.
    fn contains(&self, commit: &str, ancestor: &str) -> Result<bool, TrunkError> {
        self.git
            .is_ancestor(ancestor, commit)
            .map_err(|source| ancestry(commit, ancestor, source))
    }

    /// Whether `trunk` holds the work of the step whose tip is `head`: by
    /// history, or because every commit of the step has an equivalent patch
    /// in `trunk` (squash merges, cherry-picks). A step that committed
    /// nothing is held only by history.
    ///
    /// # Errors
    ///
    /// [`TrunkError::Ancestry`] when git fails, e.g. on an unknown commit.
    fn takes_in(&self, trunk: &str, head: &str) -> Result<bool, TrunkError> {
        if self.contains(trunk, head)? {
            return Ok(true);
        }
        let listing = self
            .git
            .run(&["cherry", trunk, head])
            .map_err(|source| ancestry(trunk, head, source))?;
        let lines: Vec<&str> = listing.lines().filter(|l| !l.trim().is_empty()).collect();
        Ok(!lines.is_empty() && lines.iter().all(|l| l.starts_with('-')))
    }
}

fn ancestry(trunk: &str, commit: &str, source: GitError) -> TrunkError {
    TrunkError::Ancestry {
        trunk: trunk.to_owned(),
        commit: commit.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspaces;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) -> String {
        Git::new(dir).run(args).expect("git")
    }

    /// A repository on `main` with one commit, an initialised graph, and a
    /// helper that realises a step as a commit on its own branch.
    struct Fixture {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
        project: Project,
        graph: Graph,
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
            Self {
                _dir: dir,
                root,
                project,
                graph,
            }
        }

        /// A completed step after `after`, committing `file` (or nothing).
        fn step(&mut self, after: NodeId, file: Option<&str>) -> NodeId {
            let id = self
                .graph
                .propose_step("step", "step", &[after], &[])
                .expect("step");
            let base = self
                .graph
                .node(after)
                .expect("after")
                .head_commit
                .expect("commit");
            let branch = Workspaces::branch_name(id);
            git(&self.root, &["branch", &branch, &base]);
            let head = if let Some(file) = file {
                let path = self.project.state_dir().join(format!("wt-{}", id.0));
                let path_text = path.display().to_string();
                git(&self.root, &["worktree", "add", "-q", &path_text, &branch]);
                std::fs::write(path.join(file), format!("{file} by {id}")).expect("write");
                git(&path, &["add", "."]);
                git(&path, &["commit", "-q", "-m", file]);
                let head = git(&path, &["rev-parse", "HEAD"]);
                git(&self.root, &["worktree", "remove", "--force", &path_text]);
                head
            } else {
                base
            };
            self.graph
                .set_branch(id, &branch, Some(&head))
                .expect("branch");
            self.graph.plan_step(id, 1).expect("plan");
            assert!(self.graph.claim(id, false).expect("claim"));
            self.graph.complete(id, "done").expect("complete");
            id
        }
    }

    #[test]
    fn following_adopts_what_the_trunk_merged() {
        let mut fx = Fixture::new();
        let origin = fx.graph.origin().expect("origin").id;
        let a = fx.step(origin, Some("a.txt"));
        let b = fx.step(a, Some("b.txt"));
        let other = fx.step(origin, Some("other.txt"));
        let noop = fx.step(origin, None);
        let trunk = Trunk::new(&fx.project, Some("main"));
        assert_eq!(trunk.follow(&mut fx.graph).expect("still"), None);

        git(
            &fx.root,
            &["merge", "-q", "--ff-only", &Workspaces::branch_name(b)],
        );
        let advance = trunk.follow(&mut fx.graph).expect("follow").expect("moved");
        assert_eq!(advance.previous, origin);
        assert!(advance.descends);
        assert_eq!(
            advance.adopted,
            vec![b],
            "a comes with b; noop changed nothing"
        );
        let lineage = fx.graph.lineage().expect("lineage");
        assert!(lineage.in_trunk(a) && lineage.in_trunk(b));
        assert!(!lineage.in_trunk(other) && !lineage.in_trunk(noop));
        assert_eq!(trunk.follow(&mut fx.graph).expect("again"), None);

        // A squash merge carries no history, but its patch is recognised.
        git(
            &fx.root,
            &["merge", "-q", "--squash", &Workspaces::branch_name(other)],
        );
        git(&fx.root, &["commit", "-q", "-m", "squashed other"]);
        let squashed = trunk.follow(&mut fx.graph).expect("follow").expect("moved");
        assert_eq!(squashed.adopted, vec![other]);
        assert_eq!(fx.graph.origin().expect("origin").title, "squashed other");

        // The owner's own work moves the trunk without adopting anything.
        std::fs::write(fx.root.join("README"), "edited\n").expect("write");
        git(&fx.root, &["commit", "-q", "-am", "edit readme"]);
        let own = trunk.follow(&mut fx.graph).expect("follow").expect("moved");
        assert!(own.adopted.is_empty());
        assert_eq!(fx.graph.origins().expect("origins").len(), 4);
    }

    #[test]
    fn a_git_failure_is_an_error_not_a_missed_adoption() {
        let mut fx = Fixture::new();
        let origin = fx.graph.origin().expect("origin").id;
        let step = fx.step(origin, Some("a.txt"));
        let gone = "0123456789abcdef0123456789abcdef01234567";
        fx.graph
            .set_branch(step, &Workspaces::branch_name(step), Some(gone))
            .expect("branch");
        std::fs::write(fx.root.join("README"), "edited\n").expect("write");
        git(&fx.root, &["commit", "-q", "-am", "edit"]);

        let trunk = Trunk::new(&fx.project, Some("main"));
        match trunk.follow(&mut fx.graph) {
            Err(TrunkError::Ancestry { commit, .. }) => assert_eq!(commit, gone),
            other => panic!("expected an ancestry error, got {other:?}"),
        }
        assert_eq!(
            fx.graph.origin().expect("origin").id,
            origin,
            "nothing is recorded while git cannot tell what was taken in"
        );
        assert!(matches!(
            trunk.contains("HEAD", gone),
            Err(TrunkError::Ancestry { .. })
        ));
        assert!(matches!(
            trunk.takes_in("HEAD", gone),
            Err(TrunkError::Ancestry { .. })
        ));
    }

    #[test]
    fn the_declared_trunk_wins_over_the_recorded_one() {
        let mut fx = Fixture::new();
        let trunk = Trunk::new(&fx.project, Some("nope"));
        assert!(matches!(
            trunk.follow(&mut fx.graph),
            Err(TrunkError::MissingBranch(_))
        ));
        // Undeclared, the branch the origin was read from (`main`) is followed.
        std::fs::write(fx.root.join("README"), "edited\n").expect("write");
        git(&fx.root, &["commit", "-q", "-am", "edit"]);
        let trunk = Trunk::new(&fx.project, None);
        assert!(trunk.follow(&mut fx.graph).expect("follow").is_some());
    }
}
