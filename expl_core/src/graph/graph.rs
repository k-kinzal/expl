//! [`Graph`] — the SQLite-backed exploration graph.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::graph::schema::{self, NOW};
use crate::graph::{
    AgentRunId, AgentRunRecord, AgentRunStatus, Edge, Evaluation, GraphError, NewEvaluation, Node,
    NodeFilter, NodeId, NodeKind, NodeStatus, Plan, PlanId, PlannedPath,
};

const NODE_COLUMNS: &str = "id, kind, status, title, objective, priority, counter_exploration, \
                            branch, head_commit, outcome, created_at, updated_at";
const EVALUATION_COLUMNS: &str = "id, node_id, goal_id, satisfied, score, reason, created_at";
const AGENT_RUN_COLUMNS: &str =
    "id, role, node_id, agent, status, log_path, session_id, error, started_at, finished_at";

/// The exploration graph. See the [module docs](crate::graph).
pub struct Graph {
    conn: Connection,
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph").finish_non_exhaustive()
    }
}

fn decode<T: std::str::FromStr>(column: &'static str, value: String) -> rusqlite::Result<T> {
    value.parse().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(CorruptValue(format!("{column}: {value}"))),
        )
    })
}

#[derive(Debug)]
struct CorruptValue(String);

impl std::fmt::Display for CorruptValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "corrupt value {}", self.0)
    }
}

impl std::error::Error for CorruptValue {}

fn node_from_row(row: &Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        id: NodeId(row.get(0)?),
        kind: decode("nodes.kind", row.get(1)?)?,
        status: decode("nodes.status", row.get(2)?)?,
        title: row.get(3)?,
        objective: row.get(4)?,
        priority: row.get(5)?,
        counter_exploration: row.get(6)?,
        branch: row.get(7)?,
        head_commit: row.get(8)?,
        outcome: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn evaluation_from_row(row: &Row<'_>) -> rusqlite::Result<Evaluation> {
    Ok(Evaluation {
        id: row.get(0)?,
        node: NodeId(row.get(1)?),
        goal: NodeId(row.get(2)?),
        satisfied: row.get(3)?,
        score: row.get(4)?,
        reason: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn agent_run_from_row(row: &Row<'_>) -> rusqlite::Result<AgentRunRecord> {
    Ok(AgentRunRecord {
        id: AgentRunId(row.get(0)?),
        role: row.get(1)?,
        node: row.get::<_, Option<i64>>(2)?.map(NodeId),
        agent: row.get(3)?,
        status: decode("agent_runs.status", row.get(4)?)?,
        log_path: row.get(5)?,
        session_id: row.get(6)?,
        error: row.get(7)?,
        started_at: row.get(8)?,
        finished_at: row.get(9)?,
    })
}

impl Graph {
    /// Open (creating and migrating as needed) the graph at `path`.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure or a too-new schema.
    pub fn open(path: &Path) -> Result<Self, GraphError> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        schema::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// An in-memory graph (tests).
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn in_memory() -> Result<Self, GraphError> {
        let mut conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", true)?;
        schema::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    // ----------------------------------------------------------------- nodes

    fn insert_node(
        &self,
        kind: NodeKind,
        status: NodeStatus,
        title: &str,
        objective: &str,
        counter_exploration: bool,
    ) -> Result<NodeId, GraphError> {
        self.conn.execute(
            "INSERT INTO nodes (kind, status, title, objective, counter_exploration) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                kind.as_str(),
                status.as_str(),
                title,
                objective,
                counter_exploration
            ],
        )?;
        Ok(NodeId(self.conn.last_insert_rowid()))
    }

    /// Create the origin at `base_commit`.
    ///
    /// # Errors
    ///
    /// [`GraphError::OriginExists`] when one already exists.
    pub fn create_origin(&self, title: &str, base_commit: &str) -> Result<NodeId, GraphError> {
        if self.origin_id()?.is_some() {
            return Err(GraphError::OriginExists);
        }
        let id = self.insert_node(
            NodeKind::Origin,
            NodeStatus::Completed,
            title,
            "The starting point of the exploration.",
            false,
        )?;
        self.conn.execute(
            "UPDATE nodes SET head_commit = ?2 WHERE id = ?1",
            params![id.0, base_commit],
        )?;
        Ok(id)
    }

    /// Add an active goal.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn add_goal(
        &self,
        title: &str,
        objective: &str,
        counter_exploration: bool,
    ) -> Result<NodeId, GraphError> {
        self.insert_node(
            NodeKind::Goal,
            NodeStatus::Active,
            title,
            objective,
            counter_exploration,
        )
    }

    /// Add a proposed step.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn add_step(&self, title: &str, objective: &str) -> Result<NodeId, GraphError> {
        self.insert_node(
            NodeKind::Step,
            NodeStatus::Proposed,
            title,
            objective,
            false,
        )
    }

    /// Add a proposed step with its edges, atomically: when any edge is
    /// invalid nothing is recorded.
    ///
    /// # Errors
    ///
    /// [`GraphError`] when an endpoint is missing or invalid, or an edge
    /// would close a cycle.
    pub fn propose_step(
        &mut self,
        title: &str,
        objective: &str,
        after: &[NodeId],
        before: &[NodeId],
    ) -> Result<NodeId, GraphError> {
        self.conn.execute_batch("SAVEPOINT propose_step")?;
        let result = (|| {
            let id = self.add_step(title, objective)?;
            for source in after {
                self.connect(*source, id)?;
            }
            for target in before {
                self.connect(id, *target)?;
            }
            Ok(id)
        })();
        match result {
            Ok(id) => {
                self.conn.execute_batch("RELEASE propose_step")?;
                Ok(id)
            }
            Err(error) => {
                self.conn
                    .execute_batch("ROLLBACK TO propose_step; RELEASE propose_step")?;
                Err(error)
            }
        }
    }

    /// The node with `id`.
    ///
    /// # Errors
    ///
    /// [`GraphError::NodeNotFound`] when absent.
    pub fn node(&self, id: NodeId) -> Result<Node, GraphError> {
        self.conn
            .query_row(
                &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = ?1"),
                [id.0],
                node_from_row,
            )
            .optional()?
            .ok_or(GraphError::NodeNotFound(id))
    }

    /// Nodes matching `filter`, by id.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn nodes(&self, filter: &NodeFilter) -> Result<Vec<Node>, GraphError> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {NODE_COLUMNS} FROM nodes ORDER BY id"))?;
        let rows = stmt.query_map([], node_from_row)?;
        let mut nodes = Vec::new();
        for row in rows {
            let node = row?;
            if (filter.kinds.is_empty() || filter.kinds.contains(&node.kind))
                && (filter.statuses.is_empty() || filter.statuses.contains(&node.status))
            {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    fn origin_id(&self) -> Result<Option<NodeId>, GraphError> {
        Ok(self
            .conn
            .query_row("SELECT id FROM nodes WHERE kind = 'origin'", [], |r| {
                r.get(0)
            })
            .optional()?
            .map(NodeId))
    }

    /// The origin.
    ///
    /// # Errors
    ///
    /// [`GraphError::NoOrigin`] when the graph is not initialised.
    pub fn origin(&self) -> Result<Node, GraphError> {
        let id = self.origin_id()?.ok_or(GraphError::NoOrigin)?;
        self.node(id)
    }

    /// Active goals.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn goals(&self) -> Result<Vec<Node>, GraphError> {
        self.nodes(&NodeFilter {
            kinds: vec![NodeKind::Goal],
            statuses: vec![NodeStatus::Active],
        })
    }

    /// Set a node's status.
    ///
    /// # Errors
    ///
    /// [`GraphError::NodeNotFound`] when absent.
    pub fn set_status(&self, id: NodeId, status: NodeStatus) -> Result<(), GraphError> {
        let changed = self.conn.execute(
            &format!("UPDATE nodes SET status = ?2, updated_at = {NOW} WHERE id = ?1"),
            params![id.0, status.as_str()],
        )?;
        if changed == 0 {
            return Err(GraphError::NodeNotFound(id));
        }
        Ok(())
    }

    /// Set a node's status and outcome together.
    ///
    /// # Errors
    ///
    /// [`GraphError::NodeNotFound`] when absent.
    pub fn conclude(
        &self,
        id: NodeId,
        status: NodeStatus,
        outcome: &str,
    ) -> Result<(), GraphError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE nodes SET status = ?2, outcome = ?3, updated_at = {NOW} WHERE id = ?1"
            ),
            params![id.0, status.as_str(), outcome],
        )?;
        if changed == 0 {
            return Err(GraphError::NodeNotFound(id));
        }
        Ok(())
    }

    /// Plan a step: give it a priority and, if it was only proposed, mark it
    /// planned. Steps in any other status keep their status.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn plan_step(&self, id: NodeId, priority: i64) -> Result<(), GraphError> {
        self.conn.execute(
            &format!(
                "UPDATE nodes SET priority = ?2, \
                 status = CASE status WHEN 'proposed' THEN 'planned' ELSE status END, \
                 updated_at = {NOW} WHERE id = ?1 AND kind = 'step'"
            ),
            params![id.0, priority],
        )?;
        Ok(())
    }

    /// Record the branch (and, when known, the tip commit) holding a node.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn set_branch(
        &self,
        id: NodeId,
        branch: &str,
        head_commit: Option<&str>,
    ) -> Result<(), GraphError> {
        self.conn.execute(
            &format!(
                "UPDATE nodes SET branch = ?2, head_commit = COALESCE(?3, head_commit), \
                 updated_at = {NOW} WHERE id = ?1"
            ),
            params![id.0, branch, head_commit],
        )?;
        Ok(())
    }

    /// Atomically move a planned step to running. `false` when it was not
    /// planned (someone else claimed it, or it changed).
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn claim(&self, id: NodeId) -> Result<bool, GraphError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE nodes SET status = 'running', updated_at = {NOW} \
                 WHERE id = ?1 AND kind = 'step' AND status = 'planned'"
            ),
            [id.0],
        )?;
        Ok(changed == 1)
    }

    /// Planned steps whose predecessors are all completed, highest priority
    /// first.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn ready_steps(&self, limit: usize) -> Result<Vec<Node>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes n \
             WHERE n.kind = 'step' AND n.status = 'planned' \
             AND NOT EXISTS (SELECT 1 FROM edges e JOIN nodes p ON p.id = e.source_id \
                             WHERE e.target_id = n.id AND p.status <> 'completed') \
             ORDER BY n.priority DESC, n.id ASC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], node_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Completed steps with no evaluation yet.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn unevaluated_steps(&self) -> Result<Vec<Node>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes n \
             WHERE n.kind = 'step' AND n.status = 'completed' \
             AND NOT EXISTS (SELECT 1 FROM evaluations v WHERE v.node_id = n.id) \
             ORDER BY n.id"
        ))?;
        let rows = stmt.query_map([], node_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Count of steps awaiting execution (`proposed` + `planned`).
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn pending_count(&self) -> Result<usize, GraphError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE kind = 'step' AND status IN ('proposed', 'planned')",
            [],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Recover from a crashed run: running steps go back to planned and
    /// running agent runs are marked interrupted. Returns recovered steps.
    ///
    /// Only safe while holding the [`ProjectLock`](crate::ProjectLock).
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn recover_interrupted(&self) -> Result<Vec<NodeId>, GraphError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM nodes WHERE kind = 'step' AND status = 'running'")?;
        let ids: Vec<NodeId> = stmt
            .query_map([], |r| r.get(0).map(NodeId))?
            .collect::<Result<_, _>>()?;
        self.conn.execute(
            &format!(
                "UPDATE nodes SET status = 'planned', updated_at = {NOW} \
                 WHERE kind = 'step' AND status = 'running'"
            ),
            [],
        )?;
        self.conn.execute(
            &format!(
                "UPDATE agent_runs SET status = 'interrupted', finished_at = {NOW} \
                 WHERE status = 'running'"
            ),
            [],
        )?;
        Ok(ids)
    }

    // ----------------------------------------------------------------- edges

    /// Add `source → target`.
    ///
    /// # Errors
    ///
    /// [`GraphError::InvalidEndpoint`] when the kinds do not allow it,
    /// [`GraphError::Cycle`] when it would close a cycle.
    pub fn connect(&self, source: NodeId, target: NodeId) -> Result<(), GraphError> {
        let source_node = self.node(source)?;
        let target_node = self.node(target)?;
        if !source_node.can_precede() {
            return Err(GraphError::InvalidEndpoint {
                node: source,
                kind: source_node.kind,
                end: "source",
            });
        }
        if !target_node.can_follow() {
            return Err(GraphError::InvalidEndpoint {
                node: target,
                kind: target_node.kind,
                end: "target",
            });
        }
        if source == target || self.reaches(target, source)? {
            return Err(GraphError::Cycle {
                from: source,
                target,
            });
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO edges (source_id, target_id) VALUES (?1, ?2)",
            params![source.0, target.0],
        )?;
        Ok(())
    }

    /// Whether `to` is reachable from `from` along edges.
    fn reaches(&self, from: NodeId, to: NodeId) -> Result<bool, GraphError> {
        Ok(self.conn.query_row(
            "WITH RECURSIVE reach(id) AS ( \
                 SELECT ?1 UNION SELECT e.target_id FROM edges e JOIN reach r ON e.source_id = r.id) \
             SELECT EXISTS (SELECT 1 FROM reach WHERE id = ?2)",
            params![from.0, to.0],
            |r| r.get(0),
        )?)
    }

    /// Every edge.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn edges(&self) -> Result<Vec<Edge>, GraphError> {
        let mut stmt = self
            .conn
            .prepare("SELECT source_id, target_id FROM edges ORDER BY target_id, source_id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Edge {
                source: NodeId(r.get(0)?),
                target: NodeId(r.get(1)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Direct predecessors of `id`.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn predecessors(&self, id: NodeId) -> Result<Vec<Node>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE id IN \
             (SELECT source_id FROM edges WHERE target_id = ?1) ORDER BY id"
        ))?;
        let rows = stmt.query_map([id.0], node_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Direct successors of `id`.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn successors(&self, id: NodeId) -> Result<Vec<Node>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE id IN \
             (SELECT target_id FROM edges WHERE source_id = ?1) ORDER BY id"
        ))?;
        let rows = stmt.query_map([id.0], node_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// All transitive predecessors of `id`, by id.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn ancestors(&self, id: NodeId) -> Result<Vec<Node>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "WITH RECURSIVE up(id) AS ( \
                 SELECT source_id FROM edges WHERE target_id = ?1 \
                 UNION SELECT e.source_id FROM edges e JOIN up u ON e.target_id = u.id) \
             SELECT {NODE_COLUMNS} FROM nodes WHERE id IN (SELECT id FROM up) ORDER BY id"
        ))?;
        let rows = stmt.query_map([id.0], node_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Active goals reachable from `id`.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn goals_reachable_from(&self, id: NodeId) -> Result<Vec<Node>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "WITH RECURSIVE down(id) AS ( \
                 SELECT target_id FROM edges WHERE source_id = ?1 \
                 UNION SELECT e.target_id FROM edges e JOIN down d ON e.source_id = d.id) \
             SELECT {NODE_COLUMNS} FROM nodes WHERE id IN (SELECT id FROM down) \
             AND kind = 'goal' AND status = 'active' ORDER BY id"
        ))?;
        let rows = stmt.query_map([id.0], node_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ----------------------------------------------------------------- plans

    /// Record a plan with its paths.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn record_plan(
        &mut self,
        summary: &str,
        paths: &[PlannedPath],
    ) -> Result<PlanId, GraphError> {
        let tx = self.conn.transaction()?;
        tx.execute("INSERT INTO plans (summary) VALUES (?1)", [summary])?;
        let plan_id = tx.last_insert_rowid();
        for path in paths {
            tx.execute(
                "INSERT INTO planned_paths (plan_id, goal_id, rationale) VALUES (?1, ?2, ?3)",
                params![plan_id, path.goal.0, path.rationale],
            )?;
            let path_id = tx.last_insert_rowid();
            for (position, node) in path.nodes.iter().enumerate() {
                tx.execute(
                    "INSERT INTO planned_path_nodes (path_id, position, node_id) \
                     VALUES (?1, ?2, ?3)",
                    params![path_id, position as i64, node.0],
                )?;
            }
        }
        tx.commit()?;
        Ok(PlanId(plan_id))
    }

    /// The most recent plan.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn latest_plan(&self) -> Result<Option<Plan>, GraphError> {
        let Some((id, summary, created_at)) = self
            .conn
            .query_row(
                "SELECT id, summary, created_at FROM plans ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT id, goal_id, rationale FROM planned_paths WHERE plan_id = ?1 ORDER BY id",
        )?;
        let heads: Vec<(i64, i64, String)> = stmt
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let mut nodes_stmt = self.conn.prepare(
            "SELECT node_id FROM planned_path_nodes WHERE path_id = ?1 ORDER BY position",
        )?;
        let mut paths = Vec::new();
        for (path_id, goal, rationale) in heads {
            let nodes = nodes_stmt
                .query_map([path_id], |r| r.get(0).map(NodeId))?
                .collect::<Result<_, _>>()?;
            paths.push(PlannedPath {
                goal: NodeId(goal),
                nodes,
                rationale,
            });
        }
        Ok(Some(Plan {
            id: PlanId(id),
            summary,
            paths,
            created_at,
        }))
    }

    /// Whether planning has something new to consider: proposed steps, or
    /// evaluations / failures since the latest plan.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn needs_planning(&self) -> Result<bool, GraphError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM nodes WHERE kind = 'step' AND status = 'proposed') \
             OR EXISTS (SELECT 1 FROM evaluations WHERE created_at > \
                        COALESCE((SELECT MAX(created_at) FROM plans), '')) \
             OR EXISTS (SELECT 1 FROM nodes WHERE kind = 'step' AND status = 'failed' \
                        AND updated_at > COALESCE((SELECT MAX(created_at) FROM plans), ''))",
            [],
            |r| r.get(0),
        )?)
    }

    // ----------------------------------------------------------- evaluations

    /// Record evaluations of `node`.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn record_evaluations(
        &mut self,
        node: NodeId,
        evaluations: &[NewEvaluation],
    ) -> Result<(), GraphError> {
        let tx = self.conn.transaction()?;
        for evaluation in evaluations {
            tx.execute(
                "INSERT INTO evaluations (node_id, goal_id, satisfied, score, reason) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    node.0,
                    evaluation.goal.0,
                    evaluation.satisfied,
                    evaluation.score.clamp(0, 100),
                    evaluation.reason
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every evaluation, oldest first.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn evaluations(&self) -> Result<Vec<Evaluation>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {EVALUATION_COLUMNS} FROM evaluations ORDER BY id"
        ))?;
        let rows = stmt.query_map([], evaluation_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Evaluations of `node`, oldest first.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn evaluations_of(&self, node: NodeId) -> Result<Vec<Evaluation>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {EVALUATION_COLUMNS} FROM evaluations WHERE node_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map([node.0], evaluation_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Goal candidates: the latest evaluation per (step, goal) pair that
    /// says the goal is satisfied, best score first.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn candidates(&self) -> Result<Vec<Evaluation>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT v.id, v.node_id, v.goal_id, v.satisfied, v.score, v.reason, v.created_at \
             FROM evaluations v JOIN nodes n ON n.id = v.node_id \
             WHERE v.id = (SELECT MAX(id) FROM evaluations w \
                           WHERE w.node_id = v.node_id AND w.goal_id = v.goal_id) \
             AND v.satisfied = 1 AND n.status = 'completed' \
             ORDER BY v.score DESC, v.id DESC",
        )?;
        let rows = stmt.query_map([], evaluation_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ------------------------------------------------------------ agent runs

    /// Journal the start of an agent run.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn begin_agent_run(
        &self,
        role: &str,
        node: Option<NodeId>,
        agent: &str,
    ) -> Result<AgentRunId, GraphError> {
        self.conn.execute(
            "INSERT INTO agent_runs (role, node_id, agent, status) VALUES (?1, ?2, ?3, 'running')",
            params![role, node.map(|n| n.0), agent],
        )?;
        Ok(AgentRunId(self.conn.last_insert_rowid()))
    }

    /// Attach the log file to an agent run.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn set_agent_run_log(&self, id: AgentRunId, log_path: &Path) -> Result<(), GraphError> {
        self.conn.execute(
            "UPDATE agent_runs SET log_path = ?2 WHERE id = ?1",
            params![id.0, log_path.display().to_string()],
        )?;
        Ok(())
    }

    /// Journal the end of an agent run.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn finish_agent_run(
        &self,
        id: AgentRunId,
        status: AgentRunStatus,
        session_id: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), GraphError> {
        self.conn.execute(
            &format!(
                "UPDATE agent_runs SET status = ?2, session_id = ?3, error = ?4, \
                 finished_at = {NOW} WHERE id = ?1"
            ),
            params![id.0, status.as_str(), session_id, error],
        )?;
        Ok(())
    }

    /// Agent runs, newest first, optionally only those about `node`.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn agent_runs(
        &self,
        node: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<AgentRunRecord>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs \
             WHERE (?1 IS NULL OR node_id = ?1) ORDER BY id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![node.map(|n| n.0), limit as i64], agent_run_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> (Graph, NodeId, NodeId) {
        let graph = Graph::in_memory().expect("graph");
        let origin = graph.create_origin("origin", "abc").expect("origin");
        let goal = graph.add_goal("goal", "do it", false).expect("goal");
        (graph, origin, goal)
    }

    #[test]
    fn single_origin() {
        let (graph, _, _) = seeded();
        assert!(matches!(
            graph.create_origin("again", "def"),
            Err(GraphError::OriginExists)
        ));
    }

    #[test]
    fn edges_reject_cycles_and_bad_endpoints() {
        let (mut graph, origin, goal) = seeded();
        let a = graph.propose_step("a", "a", &[origin], &[goal]).expect("a");
        let b = graph.propose_step("b", "b", &[a], &[]).expect("b");
        assert!(matches!(graph.connect(b, a), Err(GraphError::Cycle { .. })));
        assert!(matches!(
            graph.connect(goal, b),
            Err(GraphError::InvalidEndpoint { end: "source", .. })
        ));
        assert!(matches!(
            graph.connect(b, origin),
            Err(GraphError::InvalidEndpoint { end: "target", .. })
        ));
    }

    #[test]
    fn failed_proposal_leaves_no_trace() {
        let (mut graph, origin, goal) = seeded();
        let before = graph.nodes(&NodeFilter::default()).expect("nodes").len();
        let error = graph
            .propose_step("bad", "bad", &[origin, goal], &[])
            .expect_err("goal cannot precede");
        assert!(matches!(error, GraphError::InvalidEndpoint { .. }));
        assert_eq!(
            graph.nodes(&NodeFilter::default()).expect("nodes").len(),
            before
        );
    }

    #[test]
    fn readiness_follows_predecessors() {
        let (mut graph, origin, goal) = seeded();
        let a = graph.propose_step("a", "a", &[origin], &[]).expect("a");
        let b = graph.propose_step("b", "b", &[a], &[goal]).expect("b");
        graph.plan_step(a, 1).expect("plan a");
        graph.plan_step(b, 9).expect("plan b");
        let ready: Vec<NodeId> = graph
            .ready_steps(10)
            .expect("ready")
            .iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(ready, vec![a]);
        assert!(graph.claim(a).expect("claim"));
        assert!(!graph.claim(a).expect("second claim"));
        graph
            .conclude(a, NodeStatus::Completed, "done")
            .expect("complete");
        let ready: Vec<NodeId> = graph
            .ready_steps(10)
            .expect("ready")
            .iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(ready, vec![b]);
        assert_eq!(
            graph
                .goals_reachable_from(a)
                .expect("goals")
                .iter()
                .map(|n| n.id)
                .collect::<Vec<_>>(),
            vec![goal]
        );
        assert_eq!(graph.ancestors(b).expect("ancestors").len(), 2);
    }

    #[test]
    fn candidates_use_latest_verdict() {
        let (mut graph, origin, goal) = seeded();
        let a = graph.propose_step("a", "a", &[origin], &[goal]).expect("a");
        graph
            .conclude(a, NodeStatus::Completed, "done")
            .expect("complete");
        let verdict = |satisfied| NewEvaluation {
            goal,
            satisfied,
            score: 150,
            reason: "r".into(),
        };
        graph.record_evaluations(a, &[verdict(true)]).expect("eval");
        assert_eq!(graph.candidates().expect("candidates").len(), 1);
        assert_eq!(graph.candidates().expect("candidates")[0].score, 100);
        graph
            .record_evaluations(a, &[verdict(false)])
            .expect("eval");
        assert!(graph.candidates().expect("candidates").is_empty());
    }

    #[test]
    fn plans_round_trip() {
        let (mut graph, origin, goal) = seeded();
        let a = graph.propose_step("a", "a", &[origin], &[goal]).expect("a");
        assert!(graph.needs_planning().expect("needs"));
        graph
            .record_plan(
                "s",
                &[PlannedPath {
                    goal,
                    nodes: vec![a],
                    rationale: "r".into(),
                }],
            )
            .expect("plan");
        graph.plan_step(a, 3).expect("plan step");
        assert!(!graph.needs_planning().expect("needs"));
        let plan = graph.latest_plan().expect("latest").expect("some");
        assert_eq!(plan.paths[0].nodes, vec![a]);
        assert_eq!(graph.node(a).expect("a").status, NodeStatus::Planned);
    }
}
