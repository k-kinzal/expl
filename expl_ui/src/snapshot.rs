//! [`Snapshot`] — the data and derived relations the window shows.

use std::collections::{BTreeMap, HashMap, HashSet};

use expl_core::overview::{Overview, OverviewError};
use expl_core::{
    AgentRunRecord, AgentRunStatus, Evaluation, GoalTree, Lineage, Node, NodeId, NodeKind,
    NodeStatus, ProcessId, ProcessRecord, Project,
};
use thiserror::Error;

/// Reading a project snapshot failed.
#[derive(Debug, Error)]
pub(crate) enum SnapshotError {
    #[error(transparent)]
    Overview(#[from] OverviewError),
    #[error("could not read daemon snapshot: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not decode daemon snapshot: {0}")]
    Decode(#[from] serde_json::Error),
}

/// One view of the project, with relations derived from the returned graph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Snapshot {
    overview: Overview,
    pub(crate) lineage: Lineage,
    pub(crate) goal_tree: GoalTree,
}

impl std::ops::Deref for Snapshot {
    type Target = Overview;

    fn deref(&self) -> &Overview {
        &self.overview
    }
}

impl Snapshot {
    /// Read through the daemon in managed projects.
    pub(crate) fn load(project: &Project) -> Result<Self, SnapshotError> {
        let overview = if project.cluster_path().is_file() {
            let sibling = std::env::current_exe()?.with_file_name("expl");
            let executable = if sibling.is_file() {
                sibling
            } else {
                "expl".into()
            };
            let output = std::process::Command::new(executable)
                .arg("-C")
                .arg(project.root())
                .arg("runtime-overview")
                .output()?;
            if !output.status.success() {
                return Err(std::io::Error::other(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                )
                .into());
            }
            serde_json::from_slice::<Overview>(&output.stdout)?
        } else {
            Overview::load(project)?
        };
        let lineage = Lineage::of(&overview.nodes, &overview.edges);
        let goal_tree = GoalTree::of(&overview.nodes, &overview.edges);
        Ok(Self {
            overview,
            lineage,
            goal_tree,
        })
    }

    /// The node `id`.
    pub(crate) fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Agent runs in progress, by the node they are about.
    pub(crate) fn working_on(&self) -> HashMap<NodeId, Vec<&AgentRunRecord>> {
        let mut map: HashMap<NodeId, Vec<&AgentRunRecord>> = HashMap::new();
        for run in &self.runs {
            if run.status == AgentRunStatus::Running
                && let Some(node) = run.node
            {
                map.entry(node).or_default().push(run);
            }
        }
        map
    }

    /// The latest verdict of `node` against each goal, best score first.
    pub(crate) fn verdicts(&self, node: NodeId) -> Vec<&Evaluation> {
        let mut latest: BTreeMap<NodeId, &Evaluation> = BTreeMap::new();
        for evaluation in self.evaluations.iter().filter(|e| e.node == node) {
            latest.insert(evaluation.goal, evaluation);
        }
        let mut verdicts: Vec<_> = latest.into_values().collect();
        verdicts.sort_by(|a, b| b.score.cmp(&a.score));
        verdicts
    }

    /// Goals: the active ones first, then the retired, each set with
    /// subgoals right under the goals they serve.
    pub(crate) fn goals(&self) -> Vec<&Node> {
        let (active, retired): (Vec<&Node>, Vec<&Node>) = self
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Goal)
            .partition(|g| g.status == NodeStatus::Active);
        let mut goals = self.goal_tree.ordered(&active);
        goals.extend(self.goal_tree.ordered(&retired));
        goals
    }

    /// Where the trunk stands against `goal`: the newest origin judged
    /// against it, and that verdict.
    pub(crate) fn trunk_verdict(&self, goal: NodeId) -> Option<(NodeId, &Evaluation)> {
        self.lineage.epochs.iter().rev().find_map(|epoch| {
            self.verdicts(epoch.origin)
                .into_iter()
                .find(|e| e.goal == goal)
                .map(|e| (epoch.origin, e))
        })
    }

    /// The best-scoring hypothesis for `goal` that is not in the trunk yet:
    /// what the owner could adopt next.
    pub(crate) fn best_open(&self, goal: NodeId) -> Option<&Evaluation> {
        self.satisfying
            .iter()
            .filter(|(step, goals)| goals.contains(&goal) && !self.lineage.in_trunk(**step))
            .filter_map(|(step, _)| self.verdicts(*step).into_iter().find(|e| e.goal == goal))
            .max_by_key(|e| (e.score, e.node))
    }

    /// Whether `step` explores from an origin the trunk has moved past.
    pub(crate) fn left_behind(&self, step: NodeId) -> bool {
        !self.lineage.in_trunk(step)
            && self
                .lineage
                .explored_from(step)
                .is_some_and(|from| Some(from) != self.lineage.current)
    }

    /// The nodes that matter to `goal`: what leads to it, what was judged
    /// against it, and the trunk.
    pub(crate) fn related(&self, goal: NodeId) -> HashSet<NodeId> {
        let mut preds: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for edge in &self.edges {
            preds.entry(edge.target).or_default().push(edge.source);
        }
        let mut related: HashSet<NodeId> = HashSet::from([goal]);
        let mut stack = vec![goal];
        while let Some(id) = stack.pop() {
            for &source in preds.get(&id).into_iter().flatten() {
                if related.insert(source) {
                    stack.push(source);
                }
            }
        }
        related.extend(
            self.evaluations
                .iter()
                .filter(|e| e.goal == goal)
                .map(|e| e.node),
        );
        related.extend(
            self.nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Origin)
                .map(|n| n.id),
        );
        related
    }

    /// What each busy process is working on (`None`: no particular node).
    pub(crate) fn busy(&self) -> HashMap<ProcessId, Option<NodeId>> {
        self.runs
            .iter()
            .filter(|run| run.status == AgentRunStatus::Running)
            .filter_map(|run| Some((ProcessId(run.process?), run.node)))
            .collect()
    }

    /// The processes of `agent` that have not exited.
    pub(crate) fn processes_of<'a>(
        &'a self,
        consumer: &'a str,
    ) -> impl Iterator<Item = &'a ProcessRecord> + 'a {
        self.processes
            .iter()
            .filter(move |p| p.consumer == consumer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use expl_core::{EventKind, Graph, Inbox};

    #[test]
    fn reads_nodes_edges_events_and_satisfying() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Project::at(dir.path());
        std::fs::create_dir_all(project.state_dir()).expect("state dir");
        let mut graph = Graph::open(&project.database_path()).expect("graph");
        let origin = graph
            .create_origin("origin", Some("main"), "abc")
            .expect("origin");
        let goal = graph.add_goal("fast", "be fast", false, &[]).expect("goal");
        let step = graph
            .propose_step("cache", "add a cache", &[origin], &[goal])
            .expect("step");
        graph.plan_step(step, 5).expect("plan");
        let delivery = Inbox::open(&project.database_path(), "verifier")
            .expect("inbox")
            .lease(&[EventKind::NodeReady], ProcessId(1), false, 3)
            .expect("lease")
            .expect("node.ready");
        assert!(graph.claim(step, &delivery.token).expect("claim"));
        graph
            .complete(step, &delivery.token, "done")
            .expect("complete");
        graph
            .record_evaluations(
                step,
                &[expl_core::graph::NewEvaluation {
                    goal,
                    satisfied: true,
                    score: 90,
                    reason: "fast enough".into(),
                }],
            )
            .expect("evaluate");

        let snapshot = Snapshot::load(&project).expect("snapshot");
        assert_eq!(snapshot.nodes.len(), 3);
        assert_eq!(snapshot.satisfying.get(&step), Some(&vec![goal]));
        assert_eq!(snapshot.verdicts(step).first().map(|e| e.score), Some(90));
        assert_eq!(snapshot.best_open(goal).map(|e| e.node), Some(step));
        assert!(snapshot.trunk_verdict(goal).is_none());

        // The owner adopts the step: the trunk moves, and is judged.
        let next = graph
            .advance_origin(&expl_core::NewOrigin {
                previous: origin,
                title: "adopt cache".into(),
                note: String::new(),
                branch: Some("main".into()),
                commit: "def".into(),
                adopted: vec![step],
            })
            .expect("advance")
            .expect("moved");
        graph
            .record_evaluations(
                next,
                &[expl_core::graph::NewEvaluation {
                    goal,
                    satisfied: true,
                    score: 95,
                    reason: "fast".into(),
                }],
            )
            .expect("evaluate trunk");
        let other = graph
            .propose_step("other", "other", &[origin], &[])
            .expect("left behind");
        let snapshot = Snapshot::load(&project).expect("snapshot");
        assert_eq!(snapshot.lineage.current, Some(next));
        assert_eq!(
            snapshot.trunk_verdict(goal).map(|(o, e)| (o, e.score)),
            Some((next, 95))
        );
        assert!(
            snapshot.best_open(goal).is_none(),
            "the hypothesis is in the trunk"
        );
        assert!(!snapshot.left_behind(step));
        // A new proposal on the old origin is left behind the trunk.
        assert!(snapshot.left_behind(other));
        assert!(snapshot.related(goal).contains(&origin));
        assert_eq!(snapshot.goals().len(), 1);
        assert!(snapshot.working_on().is_empty());
        assert!(snapshot.busy().is_empty());
        assert!(snapshot.consumers.is_empty(), "no expl.toml, no consumers");
    }
}
