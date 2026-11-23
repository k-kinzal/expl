//! Read model shared by the terminal and window clients.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    AgentRunRecord, Declaration, Edge, Evaluation, Graph, GraphError, Node, NodeFilter, NodeId,
    PlannedPath, ProcessRecord, ProcessState, Processes, Project, StoreError,
};

/// How many of the newest agent runs are kept (for per-node history).
const RUN_TAIL: usize = 400;

/// Reading a snapshot failed.
#[derive(Debug, Error)]
pub enum OverviewError {
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

/// A declared consumer, as the operator wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredConsumer {
    /// `[consumer.<name>]`.
    pub name: String,
    /// Its event handler.
    pub handler: crate::Handler,
    /// How many processes `expl up` keeps.
    pub replicas: usize,
    /// Its share under legacy inline routing.
    pub weight: u32,
}

/// Data used by a project overview.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Overview {
    /// Every node, by id.
    pub nodes: Vec<Node>,
    /// Every edge.
    pub edges: Vec<Edge>,
    /// Every evaluation, oldest first.
    pub evaluations: Vec<Evaluation>,
    /// Steps that satisfy a goal → the goals they satisfy.
    pub satisfying: BTreeMap<NodeId, Vec<NodeId>>,
    /// The latest plan's paths, in the planner's order.
    pub paths: Vec<PlannedPath>,
    /// Declared consumers (empty when `expl.toml` cannot be read).
    pub consumers: Vec<DeclaredConsumer>,
    /// Processes that have not exited (running, starting, or dead).
    pub processes: Vec<ProcessRecord>,
    /// The newest agent runs, newest first.
    pub runs: Vec<AgentRunRecord>,
}

impl Overview {
    /// Read a snapshot of `project`.
    ///
    /// # Errors
    ///
    /// [`OverviewError`] when the database cannot be read.
    pub fn load(project: &Project) -> Result<Self, OverviewError> {
        let graph = Graph::open(&project.database_path())?;
        let processes = Processes::open(&project.database_path(), &project.processes_dir())?;

        let nodes = graph.nodes(&NodeFilter::default())?;
        let edges = graph.edges()?;
        let evaluations = graph.evaluations()?;
        let mut satisfying: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for evaluation in graph.satisfying()? {
            satisfying
                .entry(evaluation.node)
                .or_default()
                .push(evaluation.goal);
        }

        let runs = graph.agent_runs(None, RUN_TAIL)?;
        let paths = graph
            .latest_plan()?
            .map(|plan| plan.paths)
            .unwrap_or_default();

        let records: Vec<ProcessRecord> = processes
            .list(true)
            .map_err(|e| OverviewError::Sqlite(e.to_string()))?
            .into_iter()
            .filter(|p| p.state != ProcessState::Exited)
            .collect();

        let consumers = Declaration::load(&project.declaration_path())
            .map(|declaration| {
                declaration
                    .consumer
                    .into_iter()
                    .map(|(name, consumer)| DeclaredConsumer {
                        name,
                        handler: consumer.handler,
                        replicas: consumer.replicas,
                        weight: consumer.legacy_weight().unwrap_or(1),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            nodes,
            edges,
            evaluations,
            satisfying,
            paths,
            consumers,
            processes: records,
            runs,
        })
    }
}
