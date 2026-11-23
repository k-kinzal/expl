//! [`Trunk`] — the default branch expl follows.
//!
//! Exploration starts from the **origin**: the trunk as it stands. Agents
//! never touch the trunk. The owner moves it with git, merging the
//! hypotheses they choose and resolving conflicts as with any branch.
//! expl only follows: when the trunk's tip moves, [`Trunk::follow`] records
//! the new commit as the active origin and keeps the previous one,
//! superseded, so the graph remembers what was taken in and when
//! ([`Lineage`](crate::graph::Lineage)).
//!
//! Git is the ground truth for what was taken in. A completed step is
//! adopted when the new trunk contains its work and the previous one did
//! not: by history (a merge or fast-forward), by patch (`git cherry`:
//! squash merges and cherry-picks of the step's commits), or by content
//! (merging the step into the trunk would change nothing: a squash merge
//! of several commits, of a step together with the not-yet-adopted steps
//! it built on, or of a joined step and its merges).
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

/// One reading of the trunk: its tip as a new origin, with adoption
/// computed against the origin that was active when it was read.
#[derive(Debug, Clone)]
struct Observation {
    origin: NewOrigin,
    branch: String,
    descends: bool,
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
            git: Git::in_worktree(project.root()),
            branch: branch.filter(|b| !b.is_empty()).map(str::to_owned),
        }
    }

    /// Every branch that is or was the trunk: the declared one and each
    /// branch an origin was read from. Agents never write to these, so no
    /// step's branch may be one of them.
    ///
    /// # Errors
    ///
    /// [`GraphError`] when the origins cannot be read.
    pub fn branches(&self, graph: &Graph) -> Result<Vec<String>, GraphError> {
        let mut branches: Vec<String> = self.branch.iter().cloned().collect();
        for origin in graph.origins()? {
            if let Some(branch) = origin.branch.filter(|b| !b.is_empty())
                && !branches.contains(&branch)
            {
                branches.push(branch);
            }
        }
        Ok(branches)
    }

    /// If the trunk moved since the current origin, record its tip as the
    /// active origin, adopting the completed steps it took in. Records
    /// `origin.advanced`. `None` when there is no trunk to follow, or it
    /// has not moved (or another process recorded the move first).
    ///
    /// Adoption is computed against the active origin. If another process
    /// records a move meanwhile, this observation is stale and recording it
    /// is refused; the trunk is read again and adoption recomputed from the
    /// origin that is active now.
    ///
    /// # Errors
    ///
    /// [`TrunkError::MissingBranch`] when the trunk branch does not exist;
    /// [`TrunkError::Ancestry`] when git cannot tell what the trunk holds
    /// (nothing is recorded then); [`TrunkError::Graph`] on graph failure.
    pub fn follow(&self, graph: &mut Graph) -> Result<Option<Advance>, TrunkError> {
        let observation = self.observe(graph)?;
        self.record(graph, observation)
    }

    /// Record `observation`, reading the trunk again for as long as another
    /// process has recorded a move since the observation was made.
    fn record(
        &self,
        graph: &mut Graph,
        mut observation: Option<Observation>,
    ) -> Result<Option<Advance>, TrunkError> {
        loop {
            let Some(seen) = observation else {
                return Ok(None);
            };
            match graph.advance_origin(&seen.origin) {
                Ok(recorded) => {
                    return Ok(recorded.map(|origin| Advance {
                        origin,
                        previous: seen.origin.previous,
                        commit: seen.origin.commit,
                        branch: seen.branch,
                        adopted: seen.origin.adopted,
                        descends: seen.descends,
                    }));
                }
                Err(GraphError::OriginMoved { .. }) => observation = self.observe(graph)?,
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Read the trunk's tip and what it adopts relative to the active
    /// origin. `None` when there is no trunk to follow, or it has not moved.
    fn observe(&self, graph: &Graph) -> Result<Option<Observation>, TrunkError> {
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
        // A previous position git no longer has (the trunk was rewritten and
        // the old commit collected) is not an ancestor of anything, and held
        // nothing that can still be checked.
        let had_previous = !previous.is_empty() && self.known(&previous)?;
        let descends =
            previous.is_empty() || (had_previous && self.contains(&commit, &previous)?);

        let lineage = graph.lineage()?;
        let mut adopted: BTreeSet<NodeId> = BTreeSet::new();
        for step in graph.nodes(&NodeFilter {
            kinds: vec![NodeKind::Step],
            statuses: vec![NodeStatus::Completed],
        })? {
            let Some(head) = step.head_commit.as_deref() else {
                continue;
            };
            // An investigation changed nothing: its head commit is the code
            // it was about, which is not work the trunk could take in.
            if step.investigation || lineage.in_trunk(step.id) {
                continue;
            }
            // A step whose commit git no longer has (its branch was deleted
            // and the commit collected) cannot be shown to be in the trunk.
            // It is not taken in — and it must not stop the trunk from being
            // followed: every later move would fail on it forever.
            if !self.known(head)? {
                continue;
            }
            // A step whose tip is still a predecessor's committed nothing.
            let committed = !graph
                .predecessors(step.id)?
                .iter()
                .any(|p| p.head_commit.as_deref() == Some(head));
            let already = had_previous && self.takes_in(&previous, head, committed)?;
            if !already && self.takes_in(&commit, head, committed)? {
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
        Ok(Some(Observation {
            origin: NewOrigin {
                previous: current.id,
                title,
                note: String::new(),
                branch: Some(branch.clone()),
                commit,
                adopted: newest,
            },
            branch,
            descends,
        }))
    }

    /// Whether git has `commit` at all.
    ///
    /// # Errors
    ///
    /// [`TrunkError::Ancestry`] when git cannot be run.
    fn known(&self, commit: &str) -> Result<bool, TrunkError> {
        self.git
            .has_commit(commit)
            .map_err(|source| ancestry(commit, commit, source))
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
    /// history; because every commit of the step has an equivalent patch in
    /// `trunk` (squash merges, cherry-picks); or because merging `head`
    /// into `trunk` yields `trunk`'s own tree (squash merges of several
    /// commits, of predecessors, or of joins). A step that has not
    /// `committed` anything is held only by history.
    ///
    /// # Errors
    ///
    /// [`TrunkError::Ancestry`] when git fails, e.g. on an unknown commit.
    fn takes_in(&self, trunk: &str, head: &str, committed: bool) -> Result<bool, TrunkError> {
        if self.contains(trunk, head)? {
            return Ok(true);
        }
        if !committed {
            return Ok(false);
        }
        let failed = |source| ancestry(trunk, head, source);
        let listing = self.git.run(&["cherry", trunk, head]).map_err(failed)?;
        let lines: Vec<&str> = listing.lines().filter(|l| !l.trim().is_empty()).collect();
        if !lines.is_empty() && lines.iter().all(|l| l.starts_with('-')) {
            return Ok(true);
        }
        let Some(merged) = self.git.merge_tree(trunk, head).map_err(failed)? else {
            return Ok(false);
        };
        let tree = self
            .git
            .run(&["rev-parse", &format!("{trunk}^{{tree}}")])
            .map_err(failed)?;
        Ok(merged == tree)
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
    use crate::event::{EventKind, Inbox};
    use crate::graph::ChangeResult;
    use crate::process::ProcessId;
    use crate::role::Role;
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
            self.join(&[after], file.as_slice())
        }

        /// A completed step after every node of `after`: its branch starts
        /// at the first one's commit and merges the others' (as a joined
        /// step's workspace does), then commits each of `files` in turn.
        fn join(&mut self, after: &[NodeId], files: &[&str]) -> NodeId {
            let id = self
                .graph
                .propose_step("step", "step", after, &[])
                .expect("step");
            let commits: Vec<String> = after
                .iter()
                .map(|&n| {
                    self.graph
                        .node(n)
                        .expect("after")
                        .head_commit
                        .expect("commit")
                })
                .collect();
            let branch = Workspaces::branch_name(id);
            git(&self.root, &["branch", &branch, &commits[0]]);
            let path = self.project.state_dir().join(format!("wt-{}", id.0));
            let path_text = path.display().to_string();
            git(&self.root, &["worktree", "add", "-q", &path_text, &branch]);
            for commit in &commits[1..] {
                git(&path, &["merge", "-q", "--no-ff", "--no-edit", commit]);
            }
            for file in files {
                std::fs::write(path.join(file), format!("{file} by {id}")).expect("write");
                git(&path, &["add", "."]);
                git(&path, &["commit", "-q", "-m", file]);
            }
            let head = git(&path, &["rev-parse", "HEAD"]);
            git(&self.root, &["worktree", "remove", "--force", &path_text]);
            self.graph.plan_step(id, 1).expect("plan");
            let mut inbox = Inbox::open(&self.project.database_path(), "verifier").expect("inbox");
            let delivery = inbox
                .lease(Role::Verification.reactions(), ProcessId(1), false, 3)
                .expect("lease")
                .expect("node.ready");
            assert_eq!(delivery.event.node, Some(id));
            assert!(self.graph.claim(id, &delivery.token).expect("claim"));
            self.graph
                .publish(
                    id,
                    &delivery.token,
                    &ChangeResult {
                        branch: &branch,
                        head_commit: Some(&head),
                        completed: true,
                        outcome: "done",
                    },
                )
                .expect("publish");
            inbox.ack(&delivery, None).expect("ack");
            id
        }

        /// Squash-merge `step`'s branch into `main` as one commit.
        fn squash(&self, step: NodeId, message: &str) {
            let branch = Workspaces::branch_name(step);
            git(&self.root, &["merge", "-q", "--squash", &branch]);
            git(&self.root, &["commit", "-q", "-m", message]);
        }

        /// Whether `origin.advanced` was recorded for `origin`.
        fn advanced_to(&self, origin: NodeId) -> bool {
            self.graph
                .events(None, usize::MAX)
                .expect("events")
                .iter()
                .any(|e| e.kind == EventKind::OriginAdvanced && e.node == Some(origin))
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
    fn a_step_whose_commit_is_gone_is_not_taken_in_and_does_not_stop_following() {
        let mut fx = Fixture::new();
        let origin = fx.graph.origin().expect("origin").id;
        let step = fx.step(origin, Some("a.txt"));
        let gone = "0123456789abcdef0123456789abcdef01234567";
        crate::store::open(&fx.project.database_path())
            .expect("db")
            .execute(
                "UPDATE nodes SET head_commit = ?1 WHERE id = ?2",
                rusqlite::params![gone, step.0],
            )
            .expect("record a missing commit");
        std::fs::write(fx.root.join("README"), "edited\n").expect("write");
        git(&fx.root, &["commit", "-q", "-am", "edit"]);

        let trunk = Trunk::new(&fx.project, Some("main"));
        let advance = trunk
            .follow(&mut fx.graph)
            .expect("following survives a collected commit")
            .expect("the trunk moved");
        assert_ne!(advance.origin, origin, "the new position is recorded");
        assert!(
            !fx.graph.lineage().expect("lineage").in_trunk(step),
            "a commit git does not have is not taken in"
        );
        // Asked directly, git's failure is still an error, not `false`.
        assert!(matches!(
            trunk.contains("HEAD", gone),
            Err(TrunkError::Ancestry { .. })
        ));
        assert!(matches!(
            trunk.takes_in("HEAD", gone, true),
            Err(TrunkError::Ancestry { .. })
        ));
    }

    #[test]
    fn a_stale_observation_records_nothing() {
        let mut fx = Fixture::new();
        let first = fx.graph.origin().expect("origin").id;
        let a = fx.step(first, Some("a.txt"));
        let b = fx.step(first, Some("b.txt"));
        let c = fx.step(first, Some("c.txt"));
        // Two processes, each with its own connection to the graph.
        let mut slow = Graph::open(&fx.project.database_path()).expect("graph");
        let slow_trunk = Trunk::new(&fx.project, Some("main"));
        let fast_trunk = Trunk::new(&fx.project, Some("main"));
        let merge = |fx: &Fixture, step: NodeId| {
            let branch = Workspaces::branch_name(step);
            git(
                &fx.root,
                &["merge", "-q", "--no-ff", "-m", &branch, &branch],
            );
            git(&fx.root, &["rev-parse", "HEAD"])
        };
        let advanced = |graph: &Graph| {
            graph
                .events(None, 1000)
                .expect("events")
                .into_iter()
                .filter(|e| e.kind == EventKind::OriginAdvanced)
                .count()
        };

        // The slow follower reads the trunk after `a` was merged...
        let at_a = merge(&fx, a);
        let older = slow_trunk.observe(&slow).expect("observe").expect("moved");
        assert_eq!(
            (older.origin.previous, older.origin.commit.as_str()),
            (first, at_a.as_str())
        );
        assert_eq!(older.origin.adopted, vec![a]);

        // ...the trunk takes in `b`, and the fast one records that first.
        let at_b = merge(&fx, b);
        let newer = fast_trunk
            .follow(&mut fx.graph)
            .expect("follow")
            .expect("moved");
        assert_eq!(
            (newer.previous, newer.commit.as_str()),
            (first, at_b.as_str())
        );
        assert_eq!(newer.adopted, vec![a, b]);

        // The older observation finishes last and records nothing: no
        // origin, no adoption edge, no `origin.advanced`.
        assert!(matches!(
            slow.advance_origin(&older.origin),
            Err(GraphError::OriginMoved { expected, current })
                if expected == first && current == newer.origin
        ));
        let (origins, edges, events) = (
            slow.origins().expect("origins"),
            slow.edges().expect("edges"),
            advanced(&slow),
        );
        assert_eq!(
            slow_trunk.record(&mut slow, Some(older)).expect("record"),
            None
        );
        assert_eq!(slow.origins().expect("origins"), origins);
        assert_eq!(slow.edges().expect("edges"), edges);
        assert_eq!(advanced(&slow), events);
        assert_eq!(events, 1);
        let current = slow.origin().expect("origin");
        assert_eq!(
            (current.id, current.head_commit),
            (newer.origin, Some(at_b))
        );
        assert!(
            origins
                .iter()
                .all(|o| o.head_commit.as_deref() != Some(&at_a))
        );

        // A stale observation of a trunk that moved on again is recomputed
        // from the origin active now: `c`, not `a` again.
        let at_c = merge(&fx, c);
        let older = slow_trunk.observe(&slow).expect("observe").expect("moved");
        assert_eq!(older.origin.adopted, vec![c]);
        let latest = fast_trunk
            .follow(&mut fx.graph)
            .expect("follow")
            .expect("moved");
        assert_eq!(latest.commit, at_c);
        std::fs::write(fx.root.join("README"), "edited\n").expect("write");
        git(&fx.root, &["commit", "-q", "-am", "edit readme"]);
        let edited = git(&fx.root, &["rev-parse", "HEAD"]);
        let recomputed = slow_trunk
            .record(&mut slow, Some(older))
            .expect("record")
            .expect("moved");
        assert_eq!(recomputed.previous, latest.origin);
        assert_eq!(recomputed.commit, edited);
        assert!(recomputed.adopted.is_empty());
        assert_eq!(advanced(&slow), 3);
        assert_eq!(slow.origins().expect("origins").len(), 4);
        assert_eq!(slow.successors(c).expect("successors").len(), 1);
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

    #[test]
    fn squashing_a_step_with_its_predecessor_adopts_both() {
        let mut fx = Fixture::new();
        let origin = fx.graph.origin().expect("origin").id;
        let a = fx.step(origin, Some("a.txt"));
        let b = fx.step(a, Some("b.txt"));
        let idle = fx.step(a, None);
        let trunk = Trunk::new(&fx.project, Some("main"));

        // Neither a nor b is in the trunk; b is squashed as one commit
        // holding both, so no trunk commit matches either step's patch.
        fx.squash(b, "squashed b");
        let advance = trunk.follow(&mut fx.graph).expect("follow").expect("moved");
        assert!(fx.advanced_to(advance.origin));
        assert_eq!(
            advance.adopted,
            vec![b],
            "a comes with b; idle committed nothing"
        );
        let lineage = fx.graph.lineage().expect("lineage");
        assert!(lineage.in_trunk(a) && lineage.in_trunk(b));
        assert!(!lineage.in_trunk(idle));
        assert_eq!(trunk.follow(&mut fx.graph).expect("again"), None);
    }

    #[test]
    fn squashing_a_joined_step_adopts_it() {
        let mut fx = Fixture::new();
        let origin = fx.graph.origin().expect("origin").id;
        let a = fx.step(origin, Some("a.txt"));
        let c = fx.step(origin, Some("c.txt"));
        let joined = fx.join(&[a, c], &["j.txt"]);
        let branch = Workspaces::branch_name(joined);
        let parents = git(
            &fx.root,
            &["log", "-1", "--format=%P", &format!("{branch}~1")],
        );
        assert_eq!(parents.split_whitespace().count(), 2, "a merge commit");
        let trunk = Trunk::new(&fx.project, Some("main"));

        fx.squash(joined, "squashed join");
        let advance = trunk.follow(&mut fx.graph).expect("follow").expect("moved");
        assert!(fx.advanced_to(advance.origin));
        assert_eq!(advance.adopted, vec![joined], "a and c come with it");
        let lineage = fx.graph.lineage().expect("lineage");
        assert!(lineage.in_trunk(a) && lineage.in_trunk(c) && lineage.in_trunk(joined));
    }

    #[test]
    fn a_partial_squash_adopts_nothing_until_the_rest_follows() {
        let mut fx = Fixture::new();
        let origin = fx.graph.origin().expect("origin").id;
        let step = fx.join(&[origin], &["one.txt", "two.txt"]);
        let trunk = Trunk::new(&fx.project, Some("main"));

        // Take in the step, but leave `two.txt` out.
        git(
            &fx.root,
            &["merge", "-q", "--squash", &Workspaces::branch_name(step)],
        );
        git(&fx.root, &["rm", "-q", "--cached", "two.txt"]);
        std::fs::remove_file(fx.root.join("two.txt")).expect("remove");
        git(&fx.root, &["commit", "-q", "-m", "partial"]);
        let partial = trunk.follow(&mut fx.graph).expect("follow").expect("moved");
        assert!(partial.adopted.is_empty());
        assert!(!fx.graph.lineage().expect("lineage").in_trunk(step));

        // The rest arrives in a later commit: now the trunk holds it all.
        git(
            &fx.root,
            &["checkout", &Workspaces::branch_name(step), "--", "two.txt"],
        );
        git(&fx.root, &["commit", "-q", "-m", "the rest"]);
        let rest = trunk.follow(&mut fx.graph).expect("follow").expect("moved");
        assert_eq!(rest.adopted, vec![step]);
    }

    #[test]
    fn an_empty_commit_is_a_new_origin_and_is_evaluated() {
        let mut fx = Fixture::new();
        let trunk = Trunk::new(&fx.project, Some("main"));
        let kinds = Role::Evaluation.reactions();
        let mut evaluator = Inbox::open(&fx.project.database_path(), "evaluator").expect("inbox");
        while let Some(delivery) = evaluator
            .lease(kinds, ProcessId(1), false, crate::process::MAX_ATTEMPTS)
            .expect("lease")
        {
            evaluator.ack(&delivery, None).expect("ack");
        }
        let before = fx.graph.origin().expect("origin");
        let epochs = fx.graph.lineage().expect("lineage").epochs.len();

        // The owner moves the trunk without changing a file: a new position
        // all the same, recorded and judged like any other.
        git(
            &fx.root,
            &["commit", "-q", "--allow-empty", "-m", "empty commit"],
        );
        let tip = git(&fx.root, &["rev-parse", "main"]);
        let advance = trunk.follow(&mut fx.graph).expect("follow").expect("moved");
        assert_eq!(advance.commit, tip);
        assert_eq!(advance.previous, before.id);
        assert!(advance.descends && advance.adopted.is_empty());
        let lineage = fx.graph.lineage().expect("lineage");
        assert_eq!(lineage.epochs.len(), epochs + 1);
        assert_eq!(
            lineage.epoch(advance.origin).expect("epoch").previous,
            Some(before.id)
        );
        assert!(fx.advanced_to(advance.origin));
        let delivery = evaluator
            .lease(kinds, ProcessId(1), false, crate::process::MAX_ATTEMPTS)
            .expect("lease")
            .expect("origin.advanced delivered to evaluation");
        assert_eq!(delivery.event.kind, EventKind::OriginAdvanced);
        assert_eq!(delivery.event.node, Some(advance.origin));
    }
}
