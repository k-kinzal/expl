//! [`Snapshot`] — everything the window shows, read from the project
//! database in one go.
//!
//! The UI never holds a connection: each poll opens the database, reads a
//! snapshot, and drops it. Writers (the processes) are never blocked for
//! longer than one read, and the UI cannot drift from the graph.

use std::collections::{BTreeMap, HashMap, HashSet};

use expl_core::{
    AgentRunRecord, AgentRunStatus, Declaration, Edge, Evaluation, GoalTree, Graph, GraphError,
    Lineage, Node, NodeFilter, NodeId, NodeKind, NodeStatus, ProcessId, ProcessRecord,
    ProcessState, Processes, Project, StoreError,
};
use thiserror::Error;

/// How many of the newest agent runs are kept (for per-node history).
const RUN_TAIL: usize = 400;

/// Reading a snapshot failed.
#[derive(Debug, Error)]
pub(crate) enum SnapshotError {
    /// The graph could not be read.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// The process table could not be opened.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// SQLite failure outside the graph.
    #[error("process table: {0}")]
    Sqlite(String),
}

/// A declared agent, as the operator wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclaredAgent {
    /// `[agent.<name>]`.
    pub(crate) name: String,
    /// Its role.
    pub(crate) role: String,
    /// How many processes `expl up` keeps.
    pub(crate) replicas: usize,
    /// Its share when its role's events are routed between agents.
    pub(crate) weight: u32,
}

/// One consistent view of the project.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Snapshot {
    /// Every node, by id.
    pub(crate) nodes: Vec<Node>,
    /// Every edge.
    pub(crate) edges: Vec<Edge>,
    /// Every evaluation, oldest first.
    pub(crate) evaluations: Vec<Evaluation>,
    /// Steps that satisfy a goal → the goals they satisfy.
    pub(crate) candidates: BTreeMap<NodeId, Vec<NodeId>>,
    /// The trunk's history: every origin and what was adopted into it.
    pub(crate) lineage: Lineage,
    /// Which goals are subgoals of which.
    pub(crate) goal_tree: GoalTree,
    /// Declared agents (empty when `expl.toml` cannot be read).
    pub(crate) agents: Vec<DeclaredAgent>,
    /// Processes that have not exited (running, starting, or dead).
    pub(crate) processes: Vec<ProcessRecord>,
    /// The newest agent runs, newest first.
    pub(crate) runs: Vec<AgentRunRecord>,
}

impl Snapshot {
    /// Read a snapshot of `project`.
    ///
    /// # Errors
    ///
    /// [`SnapshotError`] when the database cannot be read.
    pub(crate) fn load(project: &Project) -> Result<Self, SnapshotError> {
        let graph = Graph::open(&project.database_path())?;
        let processes = Processes::open(&project.database_path(), &project.processes_dir())?;

        let nodes = graph.nodes(&NodeFilter::default())?;
        let edges = graph.edges()?;
        let evaluations = graph.evaluations()?;
        let mut candidates: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for evaluation in graph.candidates()? {
            candidates
                .entry(evaluation.node)
                .or_default()
                .push(evaluation.goal);
        }

        let lineage = Lineage::of(&nodes, &edges);
        let goal_tree = GoalTree::of(&nodes, &edges);
        let runs = graph.agent_runs(None, RUN_TAIL)?;

        let records: Vec<ProcessRecord> = processes
            .list(true)
            .map_err(|e| SnapshotError::Sqlite(e.to_string()))?
            .into_iter()
            .filter(|p| p.state != ProcessState::Exited)
            .collect();

        let agents = Declaration::load(&project.declaration_path())
            .map(|declaration| {
                declaration
                    .agent
                    .into_iter()
                    .map(|(name, agent)| DeclaredAgent {
                        name,
                        role: agent.role.as_str().to_owned(),
                        replicas: agent.replicas,
                        weight: agent.weight.unwrap_or(1),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            nodes,
            edges,
            evaluations,
            candidates,
            lineage,
            goal_tree,
            agents,
            processes: records,
            runs,
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

    /// The best-scoring candidate for `goal` that is not in the trunk yet:
    /// what the owner could adopt next.
    pub(crate) fn best_open(&self, goal: NodeId) -> Option<&Evaluation> {
        self.candidates
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
        agent: &'a str,
    ) -> impl Iterator<Item = &'a ProcessRecord> + 'a {
        self.processes.iter().filter(move |p| p.agent == agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nodes_edges_events_and_candidates() {
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
        assert!(graph.claim(step, false).expect("claim"));
        graph.complete(step, "done").expect("complete");
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
        assert_eq!(snapshot.candidates.get(&step), Some(&vec![goal]));
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
            "the candidate is in the trunk"
        );
        assert!(!snapshot.left_behind(step));
        // A new proposal on the old origin is left behind the trunk.
        assert!(snapshot.left_behind(other));
        assert!(snapshot.related(goal).contains(&origin));
        assert_eq!(snapshot.goals().len(), 1);
        assert!(snapshot.working_on().is_empty());
        assert!(snapshot.busy().is_empty());
        assert!(snapshot.agents.is_empty(), "no expl.toml, no agents");
    }
}
