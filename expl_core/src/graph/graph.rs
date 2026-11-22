//! [`Graph`] — the SQLite-backed exploration graph.
//!
//! Reads take `&self`; mutations take `&mut self`, run in one savepoint,
//! and record the [`Event`]s describing them before committing.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::event::{Event, EventId, EventKind, LeaseToken};
use crate::graph::{
    AgentRunId, AgentRunRecord, AgentRunStatus, Answer, Conclusion, Decision, Edge, Evaluation,
    GoalTree, GraphError, Lineage, NewConclusion, NewEvaluation, NewOrigin, Node, NodeFilter,
    NodeId, NodeKind, NodeStatus, Plan, PlanId, PlannedPath, Snapshot,
};
use crate::store::{self, EPOCH, NOW};

const NODE_COLUMNS: &str = "id, kind, status, title, objective, priority, counter_exploration, \
                            branch, head_commit, outcome, created_at, updated_at, investigation, \
                            answer, derived_from, proposed_by";
const EVALUATION_COLUMNS: &str = "id, node_id, goal_id, satisfied, score, reason, created_at";
const CONCLUSION_COLUMNS: &str = "id, node_id, decision, hypothesis, finding, consequence, \
                                  resume_commit, reopened, created_at, agent";
/// A step about to be proposed ([`Graph::propose`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct NewStep<'a> {
    /// An investigation rather than a change.
    pub investigation: bool,
    /// Short name.
    pub title: &'a str,
    /// What it must achieve, or the question it answers.
    pub objective: &'a str,
    /// Nodes it builds on.
    pub after: &'a [NodeId],
    /// Goals it heads toward.
    pub before: &'a [NodeId],
    /// The failed step it is a new hypothesis grown from
    /// ([`Node::derived_from`]).
    pub derived_from: Option<NodeId>,
    /// The exploration agent proposing it; `None` for a human.
    pub proposed_by: Option<&'a str>,
}

/// SQL condition on a predecessor `p`: it has no commit to build on yet.
/// Every origin, current or superseded, is realised.
const UNREALIZED: &str = "p.kind <> 'origin' AND p.status <> 'completed'";
const AGENT_RUN_COLUMNS: &str = "id, agent, role, process_id, event_id, node_id, status, \
                                 log_path, session_id, error, started_at, finished_at";

/// The exploration graph. See the [module docs](crate::graph).
pub struct Graph {
    conn: Connection,
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph").finish_non_exhaustive()
    }
}

fn decode<T: std::str::FromStr>(value: String) -> rusqlite::Result<T> {
    value.parse().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(format!("corrupt value `{value}`"))),
        )
    })
}

fn node_from_row(row: &Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        id: NodeId(row.get(0)?),
        kind: decode(row.get(1)?)?,
        status: decode(row.get(2)?)?,
        title: row.get(3)?,
        objective: row.get(4)?,
        priority: row.get(5)?,
        counter_exploration: row.get(6)?,
        branch: row.get(7)?,
        head_commit: row.get(8)?,
        outcome: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        investigation: row.get(12)?,
        answer: row.get::<_, Option<String>>(13)?.map(decode).transpose()?,
        derived_from: row.get::<_, Option<i64>>(14)?.map(NodeId),
        proposed_by: row.get(15)?,
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

fn conclusion_from_row(row: &Row<'_>) -> rusqlite::Result<Conclusion> {
    let decision: String = row.get(2)?;
    Ok(Conclusion {
        id: row.get(0)?,
        node: NodeId(row.get(1)?),
        decision: decision.parse().map_err(|e: String| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, e.into())
        })?,
        hypothesis: row.get(3)?,
        finding: row.get(4)?,
        consequence: row.get(5)?,
        resume_commit: row.get(6)?,
        reopened: row.get(7)?,
        created_at: row.get(8)?,
        agent: row.get(9)?,
    })
}

fn agent_run_from_row(row: &Row<'_>) -> rusqlite::Result<AgentRunRecord> {
    Ok(AgentRunRecord {
        id: AgentRunId(row.get(0)?),
        agent: row.get(1)?,
        role: row.get(2)?,
        process: row.get(3)?,
        event: row.get(4)?,
        node: row.get::<_, Option<i64>>(5)?.map(NodeId),
        status: decode(row.get(6)?)?,
        log_path: row.get(7)?,
        session_id: row.get(8)?,
        error: row.get(9)?,
        started_at: row.get(10)?,
        finished_at: row.get(11)?,
    })
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: EventId(row.get(0)?),
        kind: decode(row.get(1)?)?,
        node: row.get::<_, Option<i64>>(2)?.map(NodeId),
        created_at: row.get(3)?,
    })
}

// ------------------------------------------------- connection-level helpers

fn emit(conn: &Connection, kind: EventKind, node: Option<NodeId>) -> Result<(), GraphError> {
    conn.execute(
        "INSERT INTO events (kind, node_id) VALUES (?1, ?2)",
        params![kind.as_str(), node.map(|n| n.0)],
    )?;
    Ok(())
}

fn node_in(conn: &Connection, id: NodeId) -> Result<Node, GraphError> {
    conn.query_row(
        &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = ?1"),
        [id.0],
        node_from_row,
    )
    .optional()?
    .ok_or(GraphError::NodeNotFound(id))
}

fn nodes_where(conn: &Connection, clause: &str, id: NodeId) -> Result<Vec<Node>, GraphError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {NODE_COLUMNS} FROM nodes WHERE {clause} ORDER BY id"
    ))?;
    let rows = stmt.query_map([id.0], node_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn insert_evaluations(
    conn: &Connection,
    node: NodeId,
    evaluations: &[NewEvaluation],
) -> Result<(), GraphError> {
    for evaluation in evaluations {
        conn.execute(
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
    Ok(())
}

fn set_status_in(
    conn: &Connection,
    id: NodeId,
    status: NodeStatus,
    outcome: Option<&str>,
) -> Result<(), GraphError> {
    let changed = conn.execute(
        &format!(
            "UPDATE nodes SET status = ?2, outcome = COALESCE(?3, outcome), \
             holder_token = NULL, updated_at = {NOW} \
             WHERE id = ?1"
        ),
        params![id.0, status.as_str(), outcome],
    )?;
    if changed == 0 {
        return Err(GraphError::NodeNotFound(id));
    }
    Ok(())
}

/// Record `node.ready` when `id` is a planned step whose predecessors are
/// all completed.
fn emit_if_ready(conn: &Connection, id: NodeId) -> Result<(), GraphError> {
    let ready: bool = conn.query_row(
        &format!(
            "SELECT EXISTS (SELECT 1 FROM nodes n WHERE n.id = ?1 AND n.kind = 'step' \
             AND n.status = 'planned' AND NOT EXISTS ( \
                 SELECT 1 FROM edges e JOIN nodes p ON p.id = e.source_id \
                 WHERE e.target_id = n.id AND {UNREALIZED}))"
        ),
        [id.0],
        |r| r.get(0),
    )?;
    if ready {
        emit(conn, EventKind::NodeReady, Some(id))?;
    }
    Ok(())
}

fn emit_completion(conn: &Connection, id: NodeId) -> Result<(), GraphError> {
    emit(conn, EventKind::NodeCompleted, Some(id))?;
    for successor in nodes_where(
        conn,
        "id IN (SELECT target_id FROM edges WHERE source_id = ?1)",
        id,
    )? {
        emit_if_ready(conn, successor.id)?;
    }
    Ok(())
}

/// Check the claim and the inbox token under the graph write transaction.
/// A token can still settle its work after expiry until either the inbox
/// revokes it or another delivery takes over the step.
fn ensure_holder(conn: &Connection, id: NodeId, token: &LeaseToken) -> Result<(), GraphError> {
    let held = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM nodes n JOIN deliveries d ON d.lease_token = n.holder_token \
         WHERE n.id = ?1 AND n.status = 'running' AND n.holder_token = ?2 AND d.status = 'leased')",
        params![id.0, token.as_str()],
        |row| row.get(0),
    )?;
    if held {
        Ok(())
    } else {
        Err(GraphError::LeaseLost(id))
    }
}

/// SQL condition on a node `n`: a running step whose `node.ready` nobody
/// holds a lease on. A lease that is merely expired, or whose holder looks
/// dead, is not orphaned: the inbox delivers it again itself (counted toward
/// its attempts, and abandoned — failing the step — once they run out), and
/// its holder may only be slow. Unclaiming it as well would run the step
/// twice.
fn orphaned() -> String {
    format!(
        "n.kind = 'step' AND n.status = 'running' AND NOT EXISTS ( \
             SELECT 1 FROM deliveries d JOIN events e ON e.id = d.event_id \
             WHERE e.node_id = n.id AND e.kind = '{ready}' AND d.status = 'leased')",
        ready = EventKind::NodeReady.as_str(),
    )
}

fn current_origin_in(conn: &Connection) -> Result<Option<NodeId>, GraphError> {
    Ok(conn
        .query_row(
            "SELECT id FROM nodes WHERE kind = 'origin' AND status = 'active'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .map(NodeId))
}

fn reaches(conn: &Connection, from: NodeId, to: NodeId) -> Result<bool, GraphError> {
    Ok(conn.query_row(
        "WITH RECURSIVE reach(id) AS ( \
             SELECT ?1 UNION SELECT e.target_id FROM edges e JOIN reach r ON e.source_id = r.id) \
         SELECT EXISTS (SELECT 1 FROM reach WHERE id = ?2)",
        params![from.0, to.0],
        |r| r.get(0),
    )?)
}

fn connect_in(conn: &Connection, source: NodeId, target: NodeId) -> Result<(), GraphError> {
    let source_node = node_in(conn, source)?;
    let target_node = node_in(conn, target)?;
    // A goal precedes only the goals it is a subgoal of.
    let subgoal = source_node.kind == NodeKind::Goal && target_node.kind == NodeKind::Goal;
    if !source_node.can_precede() && !subgoal {
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
    if source == target || reaches(conn, target, source)? {
        return Err(GraphError::Cycle {
            from: source,
            target,
        });
    }
    conn.execute(
        "INSERT OR IGNORE INTO edges (source_id, target_id) VALUES (?1, ?2)",
        params![source.0, target.0],
    )?;
    Ok(())
}

impl Graph {
    /// Open (creating and migrating as needed) the graph at `path`.
    ///
    /// # Errors
    ///
    /// [`GraphError`] when the database cannot be opened.
    pub fn open(path: &Path) -> Result<Self, GraphError> {
        Ok(Self {
            conn: store::open(path)?,
        })
    }

    /// An in-memory graph (tests).
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn in_memory() -> Result<Self, GraphError> {
        Ok(Self {
            conn: store::open_in_memory()?,
        })
    }

    // ------------------------------------------------------------ mutations

    fn insert_node(
        conn: &Connection,
        kind: NodeKind,
        status: NodeStatus,
        title: &str,
        objective: &str,
        counter_exploration: bool,
    ) -> Result<NodeId, GraphError> {
        conn.execute(
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
        Ok(NodeId(conn.last_insert_rowid()))
    }

    /// Begin a write: take the database's write lock up front
    /// (`BEGIN IMMEDIATE`), waiting out other writers with the busy timeout.
    /// A deferred transaction that reads before it writes would instead fail
    /// at once (`SQLITE_BUSY_SNAPSHOT`) whenever another process committed in
    /// between, and the busy timeout cannot help it.
    fn write(&mut self) -> Result<rusqlite::Transaction<'_>, GraphError> {
        Ok(self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?)
    }

    /// Create the first origin at `base_commit`, read from the trunk
    /// `branch` (if any). The origin exists before anything happens, so it
    /// records no event.
    ///
    /// # Errors
    ///
    /// [`GraphError::OriginExists`] when the graph already has an origin.
    pub fn create_origin(
        &mut self,
        title: &str,
        branch: Option<&str>,
        base_commit: &str,
    ) -> Result<NodeId, GraphError> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM nodes WHERE kind = 'origin')",
            [],
            |r| r.get(0),
        )?;
        if exists {
            return Err(GraphError::OriginExists);
        }
        let tx = self.write()?;
        let id = Self::insert_node(
            &tx,
            NodeKind::Origin,
            NodeStatus::Active,
            title,
            "The starting point of the exploration.",
            false,
        )?;
        tx.execute(
            "UPDATE nodes SET branch = ?2, head_commit = ?3 WHERE id = ?1",
            params![id.0, branch, base_commit],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Move the trunk: record `origin` as the new active origin, superseding
    /// the current one. Records `origin.advanced`. `None` when the current
    /// origin is already at `origin.commit` — several processes follow the
    /// trunk, and only the first to see it move records it.
    ///
    /// `origin.previous` must still be the active origin: adoption is only
    /// meaningful against the origin it was computed from, and a process
    /// that observed the trunk before another recorded a newer move must
    /// not record its older observation on top.
    ///
    /// The new origin builds on the previous origin and on every adopted
    /// step, so the lineage — what was chosen, and when — stays in the
    /// graph. Every step keeps its base: whether one that has not started
    /// moves onto the new origin is planning's call ([`rebase`](Self::rebase)),
    /// since not every step still makes sense on newer code.
    ///
    /// # Errors
    ///
    /// [`GraphError::NoOrigin`] when the graph has no origin;
    /// [`GraphError::OriginMoved`] when `origin.previous` is no longer the
    /// active origin; [`GraphError::NotInStatus`] when an adopted node is
    /// not a completed step. Nothing is recorded then.
    pub fn advance_origin(&mut self, origin: &NewOrigin) -> Result<Option<NodeId>, GraphError> {
        let tx = self.write()?;
        let previous = current_origin_in(&tx)?.ok_or(GraphError::NoOrigin)?;
        if previous != origin.previous {
            return Err(GraphError::OriginMoved {
                expected: origin.previous,
                current: previous,
            });
        }
        if node_in(&tx, previous)?.head_commit.as_deref() == Some(origin.commit.as_str()) {
            return Ok(None);
        }
        for &step in &origin.adopted {
            let node = node_in(&tx, step)?;
            if node.kind != NodeKind::Step || node.status != NodeStatus::Completed {
                return Err(GraphError::NotInStatus {
                    node: step,
                    expected: "a completed step to be adopted",
                });
            }
        }
        set_status_in(&tx, previous, NodeStatus::Superseded, None)?;
        let id = Self::insert_node(
            &tx,
            NodeKind::Origin,
            NodeStatus::Active,
            &origin.title,
            &origin.note,
            false,
        )?;
        tx.execute(
            "UPDATE nodes SET branch = ?2, head_commit = ?3 WHERE id = ?1",
            params![id.0, origin.branch, origin.commit],
        )?;
        for source in std::iter::once(previous).chain(origin.adopted.iter().copied()) {
            tx.execute(
                "INSERT OR IGNORE INTO edges (source_id, target_id) VALUES (?1, ?2)",
                params![source.0, id.0],
            )?;
        }
        emit(&tx, EventKind::OriginAdvanced, Some(id))?;
        tx.commit()?;
        Ok(Some(id))
    }

    /// Add an active goal — a subgoal of `parents` when there are any (see
    /// [`GoalTree`]). Records `goal.added`.
    ///
    /// # Errors
    ///
    /// [`GraphError::NotInStatus`] when a parent is not an active goal;
    /// [`GraphError`] on SQLite failure. Nothing is recorded then.
    pub fn add_goal(
        &mut self,
        title: &str,
        objective: &str,
        counter_exploration: bool,
        parents: &[NodeId],
    ) -> Result<NodeId, GraphError> {
        let tx = self.write()?;
        for parent in parents {
            let node = node_in(&tx, *parent)?;
            if node.kind != NodeKind::Goal || node.status != NodeStatus::Active {
                return Err(GraphError::NotInStatus {
                    node: *parent,
                    expected: "an active goal",
                });
            }
        }
        let id = Self::insert_node(
            &tx,
            NodeKind::Goal,
            NodeStatus::Active,
            title,
            objective,
            counter_exploration,
        )?;
        for parent in parents {
            connect_in(&tx, id, *parent)?;
        }
        emit(&tx, EventKind::GoalAdded, Some(id))?;
        tx.commit()?;
        Ok(id)
    }

    /// Propose a step with its edges, atomically. Records `node.proposed`.
    ///
    /// # Errors
    ///
    /// [`GraphError`] when an endpoint is missing or invalid, or an edge
    /// would close a cycle; nothing is recorded then.
    pub fn propose_step(
        &mut self,
        title: &str,
        objective: &str,
        after: &[NodeId],
        before: &[NodeId],
    ) -> Result<NodeId, GraphError> {
        self.propose(&NewStep {
            title,
            objective,
            after,
            before,
            ..NewStep::default()
        })
    }

    /// Propose an investigation step: a question to answer about the code or
    /// the world, which changes nothing. Records `node.proposed`.
    ///
    /// # Errors
    ///
    /// As [`propose_step`](Self::propose_step).
    pub fn propose_investigation(
        &mut self,
        title: &str,
        objective: &str,
        after: &[NodeId],
        before: &[NodeId],
    ) -> Result<NodeId, GraphError> {
        self.propose(&NewStep {
            investigation: true,
            title,
            objective,
            after,
            before,
            ..NewStep::default()
        })
    }

    /// Propose `step` with its edges, atomically. Records `node.proposed`.
    ///
    /// # Errors
    ///
    /// As [`propose_step`](Self::propose_step); also
    /// [`GraphError::NotInStatus`] when `derived_from` is not a step.
    pub fn propose(&mut self, step: &NewStep<'_>) -> Result<NodeId, GraphError> {
        let NewStep {
            investigation,
            title,
            objective,
            after,
            before,
            derived_from,
            proposed_by,
        } = *step;
        if let Some(failed) = derived_from
            && self.node(failed)?.kind != NodeKind::Step
        {
            return Err(GraphError::NotInStatus {
                node: failed,
                expected: "a step to derive a new hypothesis from",
            });
        }
        let tx = self.write()?;
        let id = Self::insert_node(
            &tx,
            NodeKind::Step,
            NodeStatus::Proposed,
            title,
            objective,
            false,
        )?;
        if investigation {
            tx.execute("UPDATE nodes SET investigation = 1 WHERE id = ?1", [id.0])?;
        }
        if let Some(failed) = derived_from {
            tx.execute(
                "UPDATE nodes SET derived_from = ?2 WHERE id = ?1",
                [id.0, failed.0],
            )?;
        }
        if let Some(agent) = proposed_by {
            tx.execute(
                "UPDATE nodes SET proposed_by = ?2 WHERE id = ?1",
                params![id.0, agent],
            )?;
        }
        for source in after {
            connect_in(&tx, *source, id)?;
        }
        for target in before {
            connect_in(&tx, id, *target)?;
        }
        emit(&tx, EventKind::NodeProposed, Some(id))?;
        tx.commit()?;
        Ok(id)
    }

    /// Record that `id` is to be explored from again (`node.revisited`).
    ///
    /// # Errors
    ///
    /// [`GraphError::NodeNotFound`], or [`GraphError::NotInStatus`] when
    /// `id` is not one of the [`starting_points`](Self::starting_points)
    /// (with steps not carried out yet when `ahead`).
    pub fn revisit(&mut self, id: NodeId, ahead: bool) -> Result<(), GraphError> {
        if !self.starting_points(ahead)?.iter().any(|n| n.id == id) {
            self.node(id)?;
            return Err(GraphError::NotInStatus {
                node: id,
                expected: "a starting point of exploration (the active origin, a completed \
                           or failed step not in the trunk, an open step when exploring \
                           ahead, or an active counter-exploration goal)",
            });
        }
        let tx = self.write()?;
        emit(&tx, EventKind::NodeRevisited, Some(id))?;
        tx.commit()?;
        Ok(())
    }

    /// Add `source → target` to existing nodes.
    ///
    /// # Errors
    ///
    /// [`GraphError::InvalidEndpoint`] or [`GraphError::Cycle`].
    pub fn connect(&mut self, source: NodeId, target: NodeId) -> Result<(), GraphError> {
        connect_in(&self.conn, source, target)
    }

    /// Move a step that has not started off the superseded origins it
    /// builds on, onto the active origin: its work will start from the code
    /// as it is now. A step after an investigation of code the trunk has
    /// since moved past keeps the investigation (its finding still informs
    /// it) and also builds on the active origin, so its work starts from the
    /// code as it is now too. `false` when there is nothing to move.
    ///
    /// # Errors
    ///
    /// [`GraphError::NotInStatus`] unless `step` is a proposed or planned
    /// step; [`GraphError::Cycle`] when work adopted into the active origin
    /// builds on it. Nothing changes then.
    pub fn rebase(&mut self, step: NodeId) -> Result<bool, GraphError> {
        let behind = self.behind_investigations(step)?;
        let tx = self.write()?;
        let node = node_in(&tx, step)?;
        if node.kind != NodeKind::Step
            || !matches!(node.status, NodeStatus::Proposed | NodeStatus::Planned)
        {
            return Err(GraphError::NotInStatus {
                node: step,
                expected: "a proposed or planned step",
            });
        }
        let origin = current_origin_in(&tx)?.ok_or(GraphError::NoOrigin)?;
        let stale: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT e.source_id FROM edges e JOIN nodes s ON s.id = e.source_id \
                 WHERE e.target_id = ?1 AND s.kind = 'origin' AND s.id <> ?2",
            )?;
            stmt.query_map(params![step.0, origin.0], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        let on_origin: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM edges WHERE source_id = ?1 AND target_id = ?2)",
            params![origin.0, step.0],
            |r| r.get(0),
        )?;
        if stale.is_empty() && (behind.is_empty() || on_origin) {
            return Ok(false);
        }
        for source in stale {
            tx.execute(
                "DELETE FROM edges WHERE source_id = ?1 AND target_id = ?2",
                params![source, step.0],
            )?;
        }
        connect_in(&tx, origin, step)?;
        tx.commit()?;
        Ok(true)
    }

    /// The completed investigations `step` builds on that read code the
    /// trunk has since moved past: everything they built on is in the trunk
    /// (origins and adopted changes), but not the active origin.
    fn behind_investigations(&self, step: NodeId) -> Result<Vec<NodeId>, GraphError> {
        let lineage = self.lineage()?;
        let current = lineage.current;
        let mut behind = Vec::new();
        for predecessor in self.predecessors(step)? {
            if !predecessor.investigation || predecessor.status != NodeStatus::Completed {
                continue;
            }
            let ancestors = self.ancestors(predecessor.id)?;
            let settled = ancestors.iter().all(|a| match a.kind {
                NodeKind::Step => a.investigation || lineage.in_trunk(a.id),
                NodeKind::Origin | NodeKind::Goal => true,
            });
            if settled && !ancestors.iter().any(|a| Some(a.id) == current) {
                behind.push(predecessor.id);
            }
        }
        Ok(behind)
    }

    /// Make the open step `target` also build on `sources`: planning's way
    /// of combining paths (`A → C`, `B → C`). The step's branch then starts
    /// from a merge of all its predecessors. Either every edge is added or
    /// none is.
    ///
    /// # Errors
    ///
    /// [`GraphError::NotInStatus`] when `target` is not a proposed or
    /// planned step (once claimed, its branch exists and cannot change
    /// base) or a source has failed or been pruned (it would never
    /// complete); [`GraphError::InvalidEndpoint`] or [`GraphError::Cycle`]
    /// for an edge the graph cannot hold.
    pub fn join(&mut self, target: NodeId, sources: &[NodeId]) -> Result<(), GraphError> {
        let tx = self.write()?;
        let node = node_in(&tx, target)?;
        if node.kind != NodeKind::Step
            || !matches!(node.status, NodeStatus::Proposed | NodeStatus::Planned)
        {
            return Err(GraphError::NotInStatus {
                node: target,
                expected: "a proposed or planned step",
            });
        }
        for &source in sources {
            let predecessor = node_in(&tx, source)?;
            if matches!(
                predecessor.status,
                NodeStatus::Failed | NodeStatus::Pruned | NodeStatus::Superseded
            ) {
                return Err(GraphError::NotInStatus {
                    node: source,
                    expected: "the current origin or a step that has not failed or been pruned",
                });
            }
            connect_in(&tx, source, target)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Give a step a priority. A proposed step becomes planned (records
    /// `node.planned`, and `node.ready` when it can run now); a planned
    /// step is re-prioritised silently; other steps are left alone.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn plan_step(&mut self, id: NodeId, priority: i64) -> Result<(), GraphError> {
        let tx = self.write()?;
        let node = node_in(&tx, id)?;
        if node.kind == NodeKind::Step
            && matches!(node.status, NodeStatus::Proposed | NodeStatus::Planned)
        {
            tx.execute(
                &format!(
                    "UPDATE nodes SET priority = ?2, status = 'planned', updated_at = {NOW} \
                     WHERE id = ?1"
                ),
                params![id.0, priority],
            )?;
            if node.status == NodeStatus::Proposed {
                emit(&tx, EventKind::NodePlanned, Some(id))?;
                emit_if_ready(&tx, id)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Put a failed step back into the plan (records `node.planned`, and
    /// `node.ready` when it can run now).
    ///
    /// # Errors
    ///
    /// [`GraphError::NotInStatus`] unless it is a failed step.
    pub fn replan(&mut self, id: NodeId) -> Result<(), GraphError> {
        let tx = self.write()?;
        let node = node_in(&tx, id)?;
        if node.kind != NodeKind::Step || node.status != NodeStatus::Failed {
            return Err(GraphError::NotInStatus {
                node: id,
                expected: "a failed step",
            });
        }
        set_status_in(&tx, id, NodeStatus::Planned, None)?;
        emit(&tx, EventKind::NodePlanned, Some(id))?;
        emit_if_ready(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    /// Claim a ready step for the delivery identified by `token` (records
    /// `node.started`). A running step can be taken over only when its
    /// holder's delivery is no longer live. Returns `false` if the step
    /// cannot be claimed, including when a [join](Self::join) added an
    /// unrealised predecessor.
    ///
    /// # Errors
    ///
    /// [`GraphError::LeaseLost`] when the token has no live `node.ready`
    /// delivery for this step, or [`GraphError::Sqlite`] on SQLite failure.
    pub fn claim(&mut self, id: NodeId, token: &LeaseToken) -> Result<bool, GraphError> {
        let tx = self.write()?;
        let live: bool = tx.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM deliveries d JOIN events e ON e.id = d.event_id \
                 WHERE d.lease_token = ?2 AND d.status = 'leased' AND d.leased_until >= {EPOCH} \
                 AND e.kind = 'node.ready' AND e.node_id = ?1)"
            ),
            params![id.0, token.as_str()],
            |row| row.get(0),
        )?;
        if !live {
            return Err(GraphError::LeaseLost(id));
        }
        let changed = tx.execute(
            &format!(
                "UPDATE nodes SET status = 'running', holder_token = ?2, updated_at = {NOW} \
                 WHERE id = ?1 AND kind = 'step' \
                 AND (status = 'planned' OR (status = 'running' AND NOT EXISTS ( \
                     SELECT 1 FROM deliveries d WHERE d.lease_token = nodes.holder_token \
                     AND d.status = 'leased' AND d.leased_until >= {EPOCH}))) \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM edges e JOIN nodes p ON p.id = e.source_id \
                     WHERE e.target_id = ?1 AND {UNREALIZED})"
            ),
            params![id.0, token.as_str()],
        )?;
        if changed == 1 {
            emit(&tx, EventKind::NodeStarted, Some(id))?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Give a running step back to the plan without recording an event:
    /// its execution was interrupted, and the redelivered `node.ready`
    /// will claim it again.
    ///
    /// # Errors
    ///
    /// [`GraphError::LeaseLost`] when the delivery does not hold the step,
    /// or [`GraphError::Sqlite`] on SQLite failure.
    pub fn unclaim(&mut self, id: NodeId, token: &LeaseToken) -> Result<(), GraphError> {
        let tx = self.write()?;
        ensure_holder(&tx, id, token)?;
        set_status_in(&tx, id, NodeStatus::Planned, None)?;
        tx.commit()?;
        Ok(())
    }

    /// Running steps whose `node.ready` nobody holds a lease on (see
    /// `orphaned`). Their execution was abandoned, and nothing would ever
    /// take it up again.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn orphans(&self) -> Result<Vec<Node>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes n WHERE {orphaned} ORDER BY id",
            orphaned = orphaned()
        ))?;
        let rows = stmt.query_map([], node_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Give an [orphaned](Self::orphans) step back to the plan and record
    /// `node.ready` again when it can run, so execution is delivered it
    /// anew. `false`, changing nothing, when the step is not orphaned (any
    /// more): it settled, or a live process took it up.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn unclaim_orphan(&mut self, id: NodeId) -> Result<bool, GraphError> {
        let tx = self.write()?;
        let changed = tx.execute(
            &format!(
                "UPDATE nodes AS n SET status = 'planned', holder_token = NULL, updated_at = {NOW} \
                 WHERE n.id = ?1 AND {orphaned}",
                orphaned = orphaned()
            ),
            [id.0],
        )?;
        if changed == 1 {
            emit_if_ready(&tx, id)?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Record the branch (and, when known, the tip commit) of a claimed step.
    ///
    /// # Errors
    ///
    /// [`GraphError::LeaseLost`] when the delivery does not hold the step,
    /// or [`GraphError::Sqlite`] on SQLite failure.
    pub fn set_branch(
        &mut self,
        id: NodeId,
        token: &LeaseToken,
        branch: &str,
        head_commit: Option<&str>,
    ) -> Result<(), GraphError> {
        let tx = self.write()?;
        ensure_holder(&tx, id, token)?;
        tx.execute(
            &format!(
                "UPDATE nodes SET branch = ?2, head_commit = COALESCE(?3, head_commit), \
                 updated_at = {NOW} WHERE id = ?1"
            ),
            params![id.0, branch, head_commit],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A running step was realised: records `node.completed`, and
    /// `node.ready` for every successor that can run now.
    ///
    /// # Errors
    ///
    /// [`GraphError::LeaseLost`] when the delivery does not hold the step,
    /// or [`GraphError::Sqlite`] on SQLite failure.
    pub fn complete(
        &mut self,
        id: NodeId,
        token: &LeaseToken,
        outcome: &str,
    ) -> Result<(), GraphError> {
        let tx = self.write()?;
        ensure_holder(&tx, id, token)?;
        set_status_in(&tx, id, NodeStatus::Completed, Some(outcome))?;
        emit_completion(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    /// A running investigation found its answer: it completes at `code` —
    /// the commit it was about, which its successors build on — with no
    /// branch, and `evidence` as its outcome. Validates the delivery's claim
    /// on the step and records `node.completed` and any
    /// successors' `node.ready` events in the same transaction.
    ///
    /// # Errors
    ///
    /// [`GraphError::LeaseLost`] when the delivery does not hold the step,
    /// or [`GraphError::NotInStatus`] when the held step is not an investigation.
    /// SQLite failures are returned as [`GraphError::Sqlite`].
    pub fn record_finding(
        &mut self,
        id: NodeId,
        token: &LeaseToken,
        code: &str,
        answer: Answer,
        evidence: &str,
    ) -> Result<(), GraphError> {
        let tx = self.write()?;
        ensure_holder(&tx, id, token)?;
        let changed = tx.execute(
            &format!(
                "UPDATE nodes SET status = 'completed', answer = ?2, outcome = ?3, \
                 head_commit = ?4, branch = NULL, holder_token = NULL, updated_at = {NOW} \
                 WHERE id = ?1 AND kind = 'step' AND investigation = 1 AND status = 'running'"
            ),
            params![id.0, answer.as_str(), evidence, code],
        )?;
        if changed == 0 {
            return Err(GraphError::NotInStatus {
                node: id,
                expected: "a running investigation",
            });
        }
        emit_completion(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    /// A running step failed: records `node.failed`.
    ///
    /// # Errors
    ///
    /// [`GraphError::LeaseLost`] when the delivery does not hold the step,
    /// or [`GraphError::Sqlite`] on SQLite failure.
    pub fn fail(
        &mut self,
        id: NodeId,
        token: &LeaseToken,
        outcome: &str,
    ) -> Result<(), GraphError> {
        let tx = self.write()?;
        ensure_holder(&tx, id, token)?;
        set_status_in(&tx, id, NodeStatus::Failed, Some(outcome))?;
        emit(&tx, EventKind::NodeFailed, Some(id))?;
        tx.commit()?;
        Ok(())
    }

    /// Discard a step (records `node.pruned`) or retire a goal (records
    /// `goal.retired`).
    ///
    /// # Errors
    ///
    /// [`GraphError::NotInStatus`] for the origin, running or completed
    /// steps, and goals that are not active.
    pub fn prune(&mut self, id: NodeId, reason: &str) -> Result<(), GraphError> {
        let tx = self.write()?;
        let node = node_in(&tx, id)?;
        let kind = match (node.kind, node.status) {
            (NodeKind::Goal, NodeStatus::Active) => EventKind::GoalRetired,
            (NodeKind::Step, NodeStatus::Proposed | NodeStatus::Planned | NodeStatus::Failed) => {
                EventKind::NodePruned
            }
            _ => {
                return Err(GraphError::NotInStatus {
                    node: id,
                    expected: "an active goal, or a proposed, planned, or failed step",
                });
            }
        };
        set_status_in(&tx, id, NodeStatus::Pruned, Some(reason))?;
        emit(&tx, kind, Some(id))?;
        tx.commit()?;
        Ok(())
    }

    /// Record a plan with its paths (records `plan.recorded`).
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn record_plan(
        &mut self,
        summary: &str,
        paths: &[PlannedPath],
    ) -> Result<PlanId, GraphError> {
        let tx = self.write()?;
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
        emit(&tx, EventKind::PlanRecorded, None)?;
        tx.commit()?;
        Ok(PlanId(plan_id))
    }

    /// Record evaluations of `node` (records `node.evaluated`).
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn record_evaluations(
        &mut self,
        node: NodeId,
        evaluations: &[NewEvaluation],
    ) -> Result<(), GraphError> {
        let tx = self.write()?;
        insert_evaluations(&tx, node, evaluations)?;
        emit(&tx, EventKind::NodeEvaluated, Some(node))?;
        tx.commit()?;
        Ok(())
    }

    /// Record what evaluation concluded about the step `node`, judged on
    /// the strength of `event` (its `node.completed` or `node.failed`), with
    /// its verdicts against the goals, in one transaction.
    ///
    /// Nothing is recorded when the step was started again after `event`:
    /// the judgement is about an attempt that no longer stands, and the new
    /// attempt gets its own. Returns `None` then.
    ///
    /// A [`Decision::Continue`] puts the step back to run again (records
    /// `node.planned`, and `node.ready` when it can run now): its answer is
    /// cleared, and a change step's next attempt starts from the commit
    /// this one left. It is not put back when it is no longer completed or
    /// failed (someone pruned or replanned it meanwhile), when a step built
    /// on it is running or completed (their work stands on this attempt),
    /// or when the trunk adopted it. The verdicts are recorded unless it is
    /// put back. Anything else, and a continue not put back, records
    /// `node.evaluated`, so exploration and planning take it from there.
    /// Returns whether the step was put back.
    ///
    /// # Errors
    ///
    /// [`GraphError::NotInStatus`] when `node` is not a step;
    /// [`GraphError`] on SQLite failure.
    pub fn conclude(
        &mut self,
        node: NodeId,
        event: EventId,
        conclusion: &NewConclusion,
        evaluations: &[NewEvaluation],
    ) -> Result<Option<bool>, GraphError> {
        let tx = self.write()?;
        let step = node_in(&tx, node)?;
        if step.kind != NodeKind::Step {
            return Err(GraphError::NotInStatus {
                node,
                expected: "a step",
            });
        }
        let restarted: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM events WHERE node_id = ?1 AND kind = ?2 AND id > ?3)",
            params![node.0, EventKind::NodeStarted.as_str(), event.0],
            |row| row.get(0),
        )?;
        if restarted {
            return Ok(None);
        }
        let built_on: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM edges e JOIN nodes s ON s.id = e.target_id \
             WHERE e.source_id = ?1 AND (s.kind = 'origin' \
             OR (s.kind = 'step' AND s.status IN ('running', 'completed'))))",
            [node.0],
            |row| row.get(0),
        )?;
        let reopen = conclusion.decision == Decision::Continue
            && matches!(step.status, NodeStatus::Completed | NodeStatus::Failed)
            && !built_on;
        let resume = (conclusion.decision == Decision::Continue && !step.investigation)
            .then_some(step.head_commit.as_deref())
            .flatten();
        if !reopen {
            insert_evaluations(&tx, node, evaluations)?;
        }
        tx.execute(
            "INSERT INTO conclusions \
             (node_id, decision, hypothesis, finding, consequence, resume_commit, reopened, \
              agent) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                node.0,
                conclusion.decision.as_str(),
                conclusion.hypothesis,
                conclusion.finding,
                conclusion.consequence,
                resume,
                reopen,
                conclusion.agent
            ],
        )?;
        if reopen {
            tx.execute(
                &format!(
                    "UPDATE nodes SET status = 'planned', answer = NULL, updated_at = {NOW} \
                     WHERE id = ?1"
                ),
                [node.0],
            )?;
            emit(&tx, EventKind::NodePlanned, Some(node))?;
            emit_if_ready(&tx, node)?;
        } else {
            emit(&tx, EventKind::NodeEvaluated, Some(node))?;
        }
        tx.commit()?;
        Ok(Some(reopen))
    }

    // ----------------------------------------------------------------- reads

    /// What evaluation concluded about `node`, oldest first.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn conclusions_of(&self, node: NodeId) -> Result<Vec<Conclusion>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CONCLUSION_COLUMNS} FROM conclusions WHERE node_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map([node.0], conclusion_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every conclusion, oldest first.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn conclusions(&self) -> Result<Vec<Conclusion>, GraphError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CONCLUSION_COLUMNS} FROM conclusions ORDER BY id"
        ))?;
        let rows = stmt.query_map([], conclusion_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The node with `id`.
    ///
    /// # Errors
    ///
    /// [`GraphError::NodeNotFound`] when absent.
    pub fn node(&self, id: NodeId) -> Result<Node, GraphError> {
        node_in(&self.conn, id)
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

    /// The current (active) origin: where exploration starts now.
    ///
    /// # Errors
    ///
    /// [`GraphError::NoOrigin`] when the graph is not initialised.
    pub fn origin(&self) -> Result<Node, GraphError> {
        let id = current_origin_in(&self.conn)?.ok_or(GraphError::NoOrigin)?;
        self.node(id)
    }

    /// Every origin, oldest first: the positions the trunk has held.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn origins(&self) -> Result<Vec<Node>, GraphError> {
        self.nodes(&NodeFilter {
            kinds: vec![NodeKind::Origin],
            statuses: Vec::new(),
        })
    }

    /// The trunk's history: each origin with what was adopted into it.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn lineage(&self) -> Result<Lineage, GraphError> {
        Ok(Lineage::of(
            &self.nodes(&NodeFilter::default())?,
            &self.edges()?,
        ))
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

    /// Every node exploration can start from again: the active origin,
    /// each completed step whose work is not yet in the trunk (forward),
    /// each failed step (new hypotheses in its place), each active
    /// counter-exploration goal (backward) and, when `ahead`, each step not
    /// carried out yet (explored assuming it succeeds). Empty while there
    /// are no active goals.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn starting_points(&self, ahead: bool) -> Result<Vec<Node>, GraphError> {
        let goals = self.goals()?;
        if goals.is_empty() {
            return Ok(Vec::new());
        }
        let lineage = self.lineage()?;
        let mut points = vec![self.origin()?];
        points.extend(
            self.nodes(&NodeFilter {
                kinds: vec![NodeKind::Step],
                statuses: if ahead {
                    vec![
                        NodeStatus::Proposed,
                        NodeStatus::Planned,
                        NodeStatus::Completed,
                        NodeStatus::Failed,
                    ]
                } else {
                    vec![NodeStatus::Completed, NodeStatus::Failed]
                },
            })?
            .into_iter()
            .filter(|step| !lineage.in_trunk(step.id)),
        );
        points.extend(goals.into_iter().filter(|g| g.counter_exploration));
        Ok(points)
    }

    /// Steps awaiting execution (`proposed` + `planned`).
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

    /// Which goals are subgoals of which.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn goal_tree(&self) -> Result<GoalTree, GraphError> {
        let goals = self.nodes(&NodeFilter {
            kinds: vec![NodeKind::Goal],
            statuses: Vec::new(),
        })?;
        Ok(GoalTree::of(&goals, &self.edges()?))
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
        nodes_where(
            &self.conn,
            "id IN (SELECT source_id FROM edges WHERE target_id = ?1)",
            id,
        )
    }

    /// The steps proposed as new hypotheses grown from the failed step
    /// `id` ([`Node::derived_from`]).
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn derived_from(&self, id: NodeId) -> Result<Vec<Node>, GraphError> {
        nodes_where(&self.conn, "derived_from = ?1", id)
    }

    /// Direct successors of `id`.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn successors(&self, id: NodeId) -> Result<Vec<Node>, GraphError> {
        nodes_where(
            &self.conn,
            "id IN (SELECT target_id FROM edges WHERE source_id = ?1)",
            id,
        )
    }

    /// All transitive predecessors of `id`, by id.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn ancestors(&self, id: NodeId) -> Result<Vec<Node>, GraphError> {
        nodes_where(
            &self.conn,
            "id IN (WITH RECURSIVE up(id) AS ( \
                 SELECT source_id FROM edges WHERE target_id = ?1 \
                 UNION SELECT e.source_id FROM edges e JOIN up u ON e.target_id = u.id) \
             SELECT id FROM up)",
            id,
        )
    }

    /// Active goals reachable from `id` without passing through an origin
    /// (a step adopted into the trunk does not inherit the goals of the
    /// steps explored after it).
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn goals_reachable_from(&self, id: NodeId) -> Result<Vec<Node>, GraphError> {
        nodes_where(
            &self.conn,
            "kind = 'goal' AND status = 'active' AND id IN (WITH RECURSIVE down(id) AS ( \
                 SELECT e.target_id FROM edges e JOIN nodes t ON t.id = e.target_id \
                 WHERE e.source_id = ?1 AND t.kind <> 'origin' \
                 UNION SELECT e.target_id FROM edges e JOIN down d ON e.source_id = d.id \
                 JOIN nodes t ON t.id = e.target_id WHERE t.kind <> 'origin') \
             SELECT id FROM down)",
            id,
        )
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
             AND v.satisfied = 1 AND n.kind = 'step' AND n.status = 'completed' \
             ORDER BY v.score DESC, v.id DESC",
        )?;
        let rows = stmt.query_map([], evaluation_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Events after `after` (all when `None`), oldest first, at most
    /// `limit`.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn events(&self, after: Option<EventId>, limit: usize) -> Result<Vec<Event>, GraphError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, node_id, created_at FROM events WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![after.map_or(0, |e| e.0), limit as i64],
            event_from_row,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The newest event id.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn last_event(&self) -> Result<Option<EventId>, GraphError> {
        Ok(self
            .conn
            .query_row("SELECT MAX(id) FROM events", [], |r| {
                r.get::<_, Option<i64>>(0)
            })?
            .map(EventId))
    }

    /// Every node, edge, and evaluation with the newest event, read in one
    /// transaction so that no mutation lands between them.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn snapshot(&self) -> Result<Snapshot, GraphError> {
        let tx = self.conn.unchecked_transaction()?;
        let snapshot = Snapshot {
            nodes: self.nodes(&NodeFilter::default())?,
            edges: self.edges()?,
            evaluations: self.evaluations()?,
            conclusions: self.conclusions()?,
            through: self.last_event()?,
        };
        tx.commit()?;
        Ok(snapshot)
    }

    // ------------------------------------------------------------ agent runs

    /// Journal the start of an agent run.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn begin_agent_run(
        &mut self,
        agent: &str,
        role: &str,
        process: Option<i64>,
        event: Option<EventId>,
        node: Option<NodeId>,
    ) -> Result<AgentRunId, GraphError> {
        self.conn.execute(
            "INSERT INTO agent_runs (agent, role, process_id, event_id, node_id, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'running')",
            params![agent, role, process, event.map(|e| e.0), node.map(|n| n.0)],
        )?;
        Ok(AgentRunId(self.conn.last_insert_rowid()))
    }

    /// Attach the log file to an agent run.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn set_agent_run_log(&mut self, id: AgentRunId, log_path: &Path) -> Result<(), GraphError> {
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
        &mut self,
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

    /// When the newest run of `agent` that succeeded started (UTC, RFC
    /// 3339, comparable with event times), if it ever succeeded.
    ///
    /// # Errors
    ///
    /// [`GraphError`] on SQLite failure.
    pub fn last_success(&self, agent: &str) -> Result<Option<String>, GraphError> {
        Ok(self
            .conn
            .query_row(
                "SELECT started_at FROM agent_runs WHERE agent = ?1 AND status = 'succeeded' \
                 ORDER BY id DESC LIMIT 1",
                params![agent],
                |r| r.get(0),
            )
            .optional()?)
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
    use crate::event::{Delivery, Inbox};
    use crate::process::ProcessId;

    const EXECUTOR: &str = "executor";

    fn seeded() -> (Graph, NodeId, NodeId) {
        seed(Graph::in_memory().expect("graph"))
    }

    fn seeded_with_inbox() -> (tempfile::TempDir, Graph, NodeId, NodeId, Inbox) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let (graph, origin, goal) = seed(Graph::open(&db).expect("graph"));
        let inbox = Inbox::open(&db, EXECUTOR).expect("inbox");
        (dir, graph, origin, goal, inbox)
    }

    fn seed(mut graph: Graph) -> (Graph, NodeId, NodeId) {
        let origin = graph
            .create_origin("origin", Some("main"), "abc")
            .expect("origin");
        let goal = graph.add_goal("goal", "do it", false, &[]).expect("goal");
        (graph, origin, goal)
    }

    fn lease_ready(inbox: &mut Inbox, process: i64, node: NodeId) -> Delivery {
        let delivery = inbox
            .lease(&[EventKind::NodeReady], ProcessId(process), false, 3)
            .expect("lease")
            .expect("ready");
        assert_eq!(delivery.event.node, Some(node));
        delivery
    }

    fn kinds(graph: &Graph) -> Vec<(EventKind, Option<i64>)> {
        graph
            .events(None, 100)
            .expect("events")
            .into_iter()
            .map(|e| (e.kind, e.node.map(|n| n.0)))
            .collect()
    }

    #[test]
    fn only_the_current_executor_can_record_a_finding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let (mut graph, origin, goal) = seed(Graph::open(&db).expect("graph"));
        let mut inbox = Inbox::open(&db, EXECUTOR).expect("inbox");
        let question = graph
            .propose_investigation("ask", "is it so?", &[origin], &[goal])
            .expect("investigation");
        let next = graph
            .propose_step("then", "then", &[question], &[goal])
            .expect("step");
        assert!(graph.node(question).expect("node").investigation);
        assert!(!graph.node(next).expect("node").investigation);
        graph.plan_step(question, 1).expect("plan");
        graph.plan_step(next, 1).expect("plan");
        let first = lease_ready(&mut inbox, 1, question);
        let code = "abc";
        assert!(matches!(
            graph.record_finding(question, &first.token, code, Answer::Yes, "seen"),
            Err(GraphError::LeaseLost(node)) if node == question
        ));
        assert!(graph.claim(question, &first.token).expect("claim"));

        // The first replica stops renewing and the second takes its delivery.
        let expired = graph
            .conn
            .execute(
                "UPDATE deliveries SET leased_until = 0 WHERE agent = ?1 AND event_id = ?2",
                params![EXECUTOR, first.event.id.0],
            )
            .expect("expire");
        assert_eq!(expired, 1);
        let mut replacement_inbox = Inbox::open(&db, EXECUTOR).expect("inbox");
        let replacement = lease_ready(&mut replacement_inbox, 2, question);
        assert_eq!(replacement.event.id, first.event.id);
        assert_eq!(replacement.attempts, 2);
        let mut replacement_graph = Graph::open(&db).expect("replacement graph");
        assert!(
            replacement_graph
                .claim(question, &replacement.token)
                .expect("reclaim")
        );
        let running = graph.node(question).expect("running investigation");
        let before = graph.last_event().expect("last event");

        assert!(matches!(
            graph.record_finding(question, &first.token, code, Answer::No, "stale"),
            Err(GraphError::LeaseLost(node)) if node == question
        ));
        assert_eq!(graph.node(question).expect("node"), running);
        assert_eq!(graph.last_event().expect("last event"), before);

        replacement_graph
            .record_finding(question, &replacement.token, code, Answer::Yes, "seen it")
            .expect("finding");
        assert!(matches!(
            replacement_graph.record_finding(
                question,
                &replacement.token,
                code,
                Answer::No,
                "duplicate"
            ),
            Err(GraphError::LeaseLost(node)) if node == question
        ));
        let answered = graph.node(question).expect("node");
        assert_eq!(answered.status, NodeStatus::Completed);
        assert_eq!(answered.answer, Some(Answer::Yes));
        assert_eq!(answered.outcome.as_deref(), Some("seen it"));
        assert_eq!(answered.head_commit.as_deref(), Some(code));
        assert_eq!(answered.branch, None);
        let events: Vec<_> = graph
            .events(before, 100)
            .expect("events")
            .into_iter()
            .map(|event| (event.kind, event.node))
            .collect();
        assert_eq!(
            events,
            [
                (EventKind::NodeCompleted, Some(question)),
                (EventKind::NodeReady, Some(next)),
            ]
        );
        replacement_inbox.ack(&replacement, None).expect("ack");

        // A change step cannot record a finding.
        let delivery = lease_ready(&mut replacement_inbox, 2, next);
        assert!(graph.claim(next, &delivery.token).expect("claim"));
        assert!(matches!(
            graph.record_finding(next, &delivery.token, code, Answer::No, "x"),
            Err(GraphError::NotInStatus { .. })
        ));
    }

    #[test]
    fn rebasing_after_an_investigation_of_older_code_adds_the_active_origin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let (mut graph, first, goal) = seed(Graph::open(&db).expect("graph"));
        let mut inbox = Inbox::open(&db, EXECUTOR).expect("inbox");
        let change = graph.propose_step("c", "c", &[first], &[goal]).expect("c");
        let question = graph
            .propose_investigation("q", "q?", &[first], &[goal])
            .expect("q");
        let on_change = graph
            .propose_investigation("q2", "q2?", &[change], &[goal])
            .expect("q2");
        let after = graph
            .propose_step("a", "a", &[question], &[goal])
            .expect("a");
        let after_change = graph
            .propose_step("b", "b", &[on_change], &[goal])
            .expect("b");
        for step in [change, question, on_change, after, after_change] {
            graph.plan_step(step, 1).expect("plan");
        }
        let delivery = lease_ready(&mut inbox, 1, change);
        assert!(graph.claim(change, &delivery.token).expect("claim"));
        graph
            .complete(change, &delivery.token, "done")
            .expect("complete");
        inbox.ack(&delivery, None).expect("ack");
        for step in [question, on_change] {
            let delivery = lease_ready(&mut inbox, 1, step);
            assert!(graph.claim(step, &delivery.token).expect("claim"));
            graph
                .record_finding(step, &delivery.token, "abc", Answer::No, "seen")
                .expect("finding");
            inbox.ack(&delivery, None).expect("ack");
        }
        // Nothing to move while the investigation read the code as it is.
        assert!(!graph.rebase(after).expect("rebase"));

        let second = graph
            .advance_origin(&new_origin(first, "def", &[]))
            .expect("advance")
            .expect("moved");
        // It read the superseded origin's code: keep the finding, start from
        // the active origin.
        assert!(graph.rebase(after).expect("rebase"));
        let preds: Vec<NodeId> = graph
            .predecessors(after)
            .expect("preds")
            .iter()
            .map(|n| n.id)
            .collect();
        assert!(
            preds.contains(&question) && preds.contains(&second),
            "{preds:?}"
        );
        assert!(!graph.rebase(after).expect("again"), "already moved");
        // An investigation of a change the trunk has not taken in reads work
        // that is not in the active origin: that is planning's join to make.
        assert!(!graph.rebase(after_change).expect("rebase"));
        // And the trunk never takes in an investigation.
        let lineage = graph.lineage().expect("lineage");
        assert!(!lineage.in_trunk(question) && !lineage.in_trunk(on_change));
    }

    #[test]
    fn subgoals_serve_their_goals() {
        let (mut graph, origin, goal) = seeded();
        let sub = graph
            .add_goal("sub", "a route", false, &[goal])
            .expect("subgoal");
        let subsub = graph
            .add_goal("subsub", "a finer route", true, &[sub])
            .expect("subsubgoal");
        let tree = graph.goal_tree().expect("tree");
        assert_eq!(tree.ancestors(subsub), [sub, goal]);
        // A step toward a subgoal heads for the goals it serves too.
        let step = graph
            .propose_step("s", "s", &[origin], &[subsub])
            .expect("step");
        let reached: Vec<NodeId> = graph
            .goals_reachable_from(step)
            .expect("reachable")
            .iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(reached, [goal, sub, subsub]);
        // Only goals can be parents, and a goal never precedes a step.
        assert!(matches!(
            graph.add_goal("bad", "bad", false, &[step]),
            Err(GraphError::NotInStatus { node, .. }) if node == step
        ));
        assert!(matches!(
            graph.connect(goal, step),
            Err(GraphError::InvalidEndpoint { .. })
        ));
        assert!(matches!(
            graph.connect(goal, subsub),
            Err(GraphError::Cycle { .. })
        ));
    }

    #[test]
    fn starting_points_are_realised_or_failed_steps_and_counter_goals() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let mut graph = Graph::open(&db).expect("graph");
        let mut inbox = Inbox::open(&db, EXECUTOR).expect("inbox");
        let origin = graph
            .create_origin("origin", Some("main"), "abc")
            .expect("origin");
        assert!(graph.starting_points(false).expect("points").is_empty());
        let goal = graph.add_goal("goal", "do it", false, &[]).expect("goal");
        let counter = graph
            .add_goal("counter", "back", true, &[])
            .expect("counter");
        let a = graph.propose_step("a", "a", &[origin], &[goal]).expect("a");
        let b = graph.propose_step("b", "b", &[origin], &[goal]).expect("b");
        graph.plan_step(a, 1).expect("plan");
        let delivery = lease_ready(&mut inbox, 1, a);
        assert!(graph.claim(a, &delivery.token).expect("claim"));
        graph
            .set_branch(a, &delivery.token, "expl/node-a", Some("def"))
            .expect("branch");
        graph
            .complete(a, &delivery.token, "done")
            .expect("complete");
        inbox.ack(&delivery, None).expect("ack");
        let c = graph.propose_step("c", "c", &[origin], &[goal]).expect("c");
        graph.plan_step(c, 1).expect("plan");
        let delivery = lease_ready(&mut inbox, 1, c);
        assert!(graph.claim(c, &delivery.token).expect("claim"));
        graph
            .fail(c, &delivery.token, "did not compile")
            .expect("fail");

        let ids: Vec<NodeId> = graph
            .starting_points(false)
            .expect("points")
            .iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(ids, [origin, a, c, counter]);

        graph.revisit(a, false).expect("revisit a");
        graph.revisit(counter, false).expect("revisit counter");
        assert!(matches!(
            graph.revisit(b, false),
            Err(GraphError::NotInStatus { node, .. }) if node == b
        ));
        assert!(matches!(
            graph.revisit(goal, false),
            Err(GraphError::NotInStatus { .. })
        ));
        let revisits: Vec<_> = kinds(&graph)
            .into_iter()
            .filter(|(kind, _)| *kind == EventKind::NodeRevisited)
            .collect();
        assert_eq!(
            revisits,
            [
                (EventKind::NodeRevisited, Some(a.0)),
                (EventKind::NodeRevisited, Some(counter.0))
            ]
        );
        // Exploring ahead, steps not carried out yet are starting points too.
        let ahead: Vec<NodeId> = graph
            .starting_points(true)
            .expect("points")
            .iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(ahead, [origin, a, b, c, counter]);
        graph.revisit(b, true).expect("revisit b ahead");
    }

    #[test]
    fn single_origin() {
        let (mut graph, _, _) = seeded();
        assert!(matches!(
            graph.create_origin("again", None, "def"),
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
        let before_nodes = graph.nodes(&NodeFilter::default()).expect("nodes").len();
        let before_events = kinds(&graph).len();
        let error = graph
            .propose_step("bad", "bad", &[origin, goal], &[])
            .expect_err("goal cannot precede");
        assert!(matches!(error, GraphError::InvalidEndpoint { .. }));
        assert_eq!(
            graph.nodes(&NodeFilter::default()).expect("nodes").len(),
            before_nodes
        );
        assert_eq!(kinds(&graph).len(), before_events);
    }

    #[test]
    fn mutations_record_events_and_readiness() {
        let (_dir, mut graph, origin, goal, mut inbox) = seeded_with_inbox();
        let a = graph.propose_step("a", "a", &[origin], &[]).expect("a");
        let b = graph.propose_step("b", "b", &[a], &[goal]).expect("b");
        graph.plan_step(a, 1).expect("plan a");
        graph.plan_step(b, 9).expect("plan b");
        let first = lease_ready(&mut inbox, 1, a);
        assert!(graph.claim(a, &first.token).expect("claim"));
        assert!(!graph.claim(a, &first.token).expect("second claim"));
        inbox.release(&first).expect("release");
        let second = lease_ready(&mut inbox, 2, a);
        assert!(graph.claim(a, &second.token).expect("reclaim"));
        graph.complete(a, &second.token, "done").expect("complete");
        let (g, a, b) = (goal.0, a.0, b.0);
        assert_eq!(
            kinds(&graph),
            vec![
                (EventKind::GoalAdded, Some(g)),
                (EventKind::NodeProposed, Some(a)),
                (EventKind::NodeProposed, Some(b)),
                (EventKind::NodePlanned, Some(a)),
                (EventKind::NodeReady, Some(a)),
                (EventKind::NodePlanned, Some(b)),
                (EventKind::NodeStarted, Some(a)),
                (EventKind::NodeStarted, Some(a)),
                (EventKind::NodeCompleted, Some(a)),
                (EventKind::NodeReady, Some(b)),
            ]
        );
        assert_eq!(graph.ancestors(NodeId(b)).expect("ancestors").len(), 2);
        assert_eq!(
            graph
                .goals_reachable_from(NodeId(a))
                .expect("goals")
                .iter()
                .map(|n| n.id.0)
                .collect::<Vec<_>>(),
            vec![g]
        );
    }

    #[test]
    fn concurrent_claims_from_different_agents_start_the_step_once() {
        let (dir, mut graph, origin, goal, mut first_inbox) = seeded_with_inbox();
        let db = dir.path().join("expl.db");
        let mut second_inbox = Inbox::open(&db, "another-executor").expect("inbox");
        let step = graph
            .propose_step("s", "s", &[origin], &[goal])
            .expect("step");
        graph.plan_step(step, 1).expect("plan");
        let first = lease_ready(&mut first_inbox, 1, step);
        let second = lease_ready(&mut second_inbox, 2, step);
        assert_eq!(first.event.id, second.event.id);

        let start = std::sync::Barrier::new(2);
        let claimed = std::thread::scope(|scope| {
            let contenders = [&first.token, &second.token].map(|token| {
                let mut graph = Graph::open(&db).expect("graph");
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    graph.claim(step, token).expect("claim")
                })
            });
            contenders.map(|thread| thread.join().expect("contender"))
        });
        assert_ne!(claimed[0], claimed[1]);
        assert_eq!(
            kinds(&graph)
                .into_iter()
                .filter(|event| *event == (EventKind::NodeStarted, Some(step.0)))
                .count(),
            1
        );
    }

    #[test]
    fn candidates_use_latest_verdict() {
        let (_dir, mut graph, origin, goal, mut inbox) = seeded_with_inbox();
        let a = graph.propose_step("a", "a", &[origin], &[goal]).expect("a");
        graph.plan_step(a, 1).expect("plan");
        let delivery = lease_ready(&mut inbox, 1, a);
        assert!(graph.claim(a, &delivery.token).expect("claim"));
        graph
            .complete(a, &delivery.token, "done")
            .expect("complete");
        let verdict = |satisfied| NewEvaluation {
            goal,
            satisfied,
            score: 150,
            reason: "r".into(),
        };
        graph.record_evaluations(a, &[verdict(true)]).expect("eval");
        assert_eq!(graph.candidates().expect("candidates")[0].score, 100);
        graph
            .record_evaluations(a, &[verdict(false)])
            .expect("eval");
        assert!(graph.candidates().expect("candidates").is_empty());
    }

    #[test]
    fn prune_and_replan_guard_statuses() {
        let (_dir, mut graph, origin, goal, mut inbox) = seeded_with_inbox();
        let a = graph.propose_step("a", "a", &[origin], &[goal]).expect("a");
        assert!(matches!(
            graph.replan(a),
            Err(GraphError::NotInStatus { .. })
        ));
        graph.plan_step(a, 1).expect("plan");
        let delivery = lease_ready(&mut inbox, 1, a);
        assert!(graph.claim(a, &delivery.token).expect("claim"));
        graph.fail(a, &delivery.token, "x").expect("fail");
        graph.replan(a).expect("replan");
        assert_eq!(graph.node(a).expect("a").status, NodeStatus::Planned);
        graph.prune(goal, "done").expect("retire");
        assert!(graph.goals().expect("goals").is_empty());
        assert!(matches!(
            graph.prune(origin, "x"),
            Err(GraphError::NotInStatus { .. })
        ));
    }

    #[test]
    fn join_combines_open_steps_and_holds_back_their_claim() {
        let (_dir, mut graph, origin, goal, mut inbox) = seeded_with_inbox();
        let propose = |graph: &mut Graph, title: &str, after: &[NodeId]| {
            graph
                .propose_step(title, title, after, &[goal])
                .expect(title)
        };
        let ids = |graph: &Graph, id: NodeId| {
            let mut ids: Vec<NodeId> = graph
                .predecessors(id)
                .expect("predecessors")
                .iter()
                .map(|n| n.id)
                .collect();
            ids.sort();
            ids
        };
        let left = propose(&mut graph, "a", &[origin]);
        let right = propose(&mut graph, "b", &[origin]);
        let joined = propose(&mut graph, "c", &[left]);
        for step in [left, right, joined] {
            graph.plan_step(step, 1).expect("plan");
        }
        let delivery = lease_ready(&mut inbox, 1, left);
        assert!(graph.claim(left, &delivery.token).expect("claim"));
        graph.complete(left, &delivery.token, "done").expect("a");
        inbox.ack(&delivery, None).expect("ack");
        let right_delivery = lease_ready(&mut inbox, 2, right);
        let joined_delivery = lease_ready(&mut inbox, 1, joined);
        graph.join(joined, &[right]).expect("join");
        assert_eq!(ids(&graph, joined), [left, right]);

        // A joined step is claimed only once every predecessor completed.
        assert!(!graph.claim(joined, &joined_delivery.token).expect("claim"));
        assert!(graph.claim(right, &right_delivery.token).expect("claim"));
        graph
            .complete(right, &right_delivery.token, "done")
            .expect("b");
        inbox.ack(&right_delivery, None).expect("ack");
        assert!(graph.claim(joined, &joined_delivery.token).expect("claim"));
        // Once claimed, its base is fixed.
        assert!(matches!(
            graph.join(joined, &[origin]),
            Err(GraphError::NotInStatus { .. })
        ));

        // Cycles and failed sources are refused, and a refused join adds
        // no edge at all.
        let first = propose(&mut graph, "f", &[origin]);
        let second = propose(&mut graph, "g", &[first]);
        assert!(matches!(
            graph.join(first, &[second]),
            Err(GraphError::Cycle { .. })
        ));
        let failed = propose(&mut graph, "failed", &[origin]);
        graph.plan_step(failed, 1).expect("plan");
        let delivery = lease_ready(&mut inbox, 2, failed);
        assert!(graph.claim(failed, &delivery.token).expect("claim"));
        graph.fail(failed, &delivery.token, "x").expect("fail");
        assert!(matches!(
            graph.join(second, &[right, failed]),
            Err(GraphError::NotInStatus { .. })
        ));
        assert_eq!(ids(&graph, second), [first]);
    }
    fn new_origin(previous: NodeId, commit: &str, adopted: &[NodeId]) -> NewOrigin {
        NewOrigin {
            previous,
            title: "trunk".into(),
            note: String::new(),
            branch: Some("main".into()),
            commit: commit.into(),
            adopted: adopted.to_vec(),
        }
    }

    #[test]
    fn advancing_supersedes_the_origin_and_planning_rebases_open_steps() {
        let (_dir, mut graph, first, goal, mut inbox) = seeded_with_inbox();
        let adopted = graph.propose_step("a", "a", &[first], &[goal]).expect("a");
        let left = graph.propose_step("b", "b", &[first], &[goal]).expect("b");
        let open = graph.propose_step("c", "c", &[first], &[goal]).expect("c");
        let later = graph.propose_step("d", "d", &[left], &[]).expect("d");
        for step in [adopted, left, open, later] {
            graph.plan_step(step, 1).expect("plan");
        }
        for step in [adopted, left] {
            let delivery = lease_ready(&mut inbox, 1, step);
            assert!(graph.claim(step, &delivery.token).expect("claim"));
            graph
                .complete(step, &delivery.token, "done")
                .expect("complete");
            inbox.ack(&delivery, None).expect("ack");
        }
        assert!(matches!(
            graph.advance_origin(&new_origin(first, "def", &[open])),
            Err(GraphError::NotInStatus { .. })
        ));

        let second = graph
            .advance_origin(&new_origin(first, "def", &[adopted]))
            .expect("advance")
            .expect("moved");
        assert_eq!(
            graph
                .advance_origin(&new_origin(second, "def", &[]))
                .expect("again"),
            None,
            "the trunk is already there"
        );
        // An observation made from the superseded origin records nothing.
        let (edges, events) = (graph.edges().expect("edges"), kinds(&graph));
        assert!(matches!(
            graph.advance_origin(&new_origin(first, "abc", &[left])),
            Err(GraphError::OriginMoved { expected, current })
                if expected == first && current == second
        ));
        assert_eq!(graph.edges().expect("edges"), edges);
        assert_eq!(kinds(&graph), events);
        assert_eq!(graph.origin().expect("origin").id, second);
        assert_eq!(
            graph.node(first).expect("first").status,
            NodeStatus::Superseded
        );
        let ids = |nodes: Vec<Node>| nodes.into_iter().map(|n| n.id).collect::<Vec<_>>();
        assert_eq!(
            ids(graph.predecessors(second).expect("preds")),
            [first, adopted]
        );
        // Every step keeps its base when the trunk moves.
        assert_eq!(ids(graph.predecessors(open).expect("preds")), [first]);
        assert_eq!(ids(graph.predecessors(left).expect("preds")), [first]);
        assert_eq!(ids(graph.predecessors(later).expect("preds")), [left]);
        // Planning may move a step that has not started onto the new origin;
        // a step on no superseded origin has nothing to move, and only open
        // steps move.
        assert!(graph.rebase(open).expect("rebase"));
        assert_eq!(ids(graph.predecessors(open).expect("preds")), [second]);
        assert!(!graph.rebase(open).expect("nothing left to move"));
        assert!(!graph.rebase(later).expect("builds on a step"));
        assert!(matches!(
            graph.rebase(left),
            Err(GraphError::NotInStatus { .. })
        ));
        assert_eq!(
            kinds(&graph).last(),
            Some(&(EventKind::OriginAdvanced, Some(second.0)))
        );

        // Every origin is realised: steps on it can run, and a superseded
        // origin cannot be joined onto.
        let delivery = lease_ready(&mut inbox, 1, open);
        assert!(
            graph
                .claim(open, &delivery.token)
                .expect("claim on the new origin")
        );
        assert!(matches!(
            graph.join(later, &[first]),
            Err(GraphError::NotInStatus { .. })
        ));
        // Adopted work does not inherit the goals explored after it.
        let next = graph.propose_step("e", "e", &[second], &[]).expect("e");
        let other = graph.add_goal("other", "other", false, &[]).expect("goal");
        graph.connect(next, other).expect("edge");
        assert_eq!(
            ids(graph.goals_reachable_from(adopted).expect("goals")),
            [goal]
        );

        let lineage = graph.lineage().expect("lineage");
        assert_eq!(lineage.current, Some(second));
        assert!(lineage.in_trunk(adopted));
        assert_eq!(lineage.explored_from(open), Some(second));
        assert_eq!(graph.origins().expect("origins").len(), 2);
    }

    #[test]
    fn origin_verdicts_are_not_candidates() {
        let (mut graph, origin, goal) = seeded();
        graph
            .record_evaluations(
                origin,
                &[NewEvaluation {
                    goal,
                    satisfied: true,
                    score: 100,
                    reason: "already there".into(),
                }],
            )
            .expect("eval");
        assert!(graph.candidates().expect("candidates").is_empty());
    }

    fn concluded(decision: Decision) -> NewConclusion {
        NewConclusion {
            agent: "evaluator".to_owned(),
            decision,
            hypothesis: "writing hello works".to_owned(),
            finding: "stopped at the compiler".to_owned(),
            consequence: "finish hello.rs".to_owned(),
        }
    }

    /// The newest event: what an evaluation of a step just settled reacts to.
    fn newest(graph: &Graph) -> EventId {
        graph.last_event().expect("events").expect("an event")
    }

    /// A step that ran, left `commit`, and `failed` or completed.
    fn ran(
        graph: &mut Graph,
        inbox: &mut Inbox,
        origin: NodeId,
        goal: NodeId,
        failed: bool,
    ) -> NodeId {
        let step = graph
            .propose_step("hello", "write hello", &[origin], &[goal])
            .expect("step");
        graph.plan_step(step, 1).expect("plan");
        let delivery = lease_ready(inbox, 1, step);
        assert!(graph.claim(step, &delivery.token).expect("claim"));
        graph
            .set_branch(step, &delivery.token, "expl/node-3", Some("left"))
            .expect("branch");
        if failed {
            graph
                .fail(step, &delivery.token, "timed out")
                .expect("fail");
        } else {
            graph
                .complete(step, &delivery.token, "done, it says")
                .expect("complete");
        }
        inbox.ack(&delivery, None).expect("ack");
        step
    }

    #[test]
    fn a_step_continued_runs_again_from_what_it_left() {
        let (_dir, mut graph, origin, goal, mut inbox) = seeded_with_inbox();
        let step = ran(&mut graph, &mut inbox, origin, goal, true);

        let reopened = graph
            .conclude(step, newest(&graph), &concluded(Decision::Continue), &[])
            .expect("conclude");

        assert_eq!(reopened, Some(true));
        assert_eq!(graph.node(step).expect("node").status, NodeStatus::Planned);
        let tail: Vec<_> = kinds(&graph).into_iter().rev().take(2).collect();
        assert_eq!(
            tail,
            [
                (EventKind::NodeReady, Some(step.0)),
                (EventKind::NodePlanned, Some(step.0))
            ]
        );
        let conclusion = &graph.conclusions_of(step).expect("conclusions")[0];
        assert_eq!(conclusion.decision, Decision::Continue);
        assert!(conclusion.reopened);
        assert_eq!(conclusion.resume_commit.as_deref(), Some("left"));
        assert_eq!(conclusion.finding, "stopped at the compiler");
    }

    #[test]
    fn a_step_others_build_on_is_not_continued_but_explored_past() {
        let (_dir, mut graph, origin, goal, mut inbox) = seeded_with_inbox();
        let step = ran(&mut graph, &mut inbox, origin, goal, false);
        let next = graph
            .propose_step("next", "after hello", &[step], &[goal])
            .expect("next");
        graph.plan_step(next, 1).expect("plan");
        let delivery = lease_ready(&mut inbox, 1, next);
        assert!(graph.claim(next, &delivery.token).expect("claim"));

        let reopened = graph
            .conclude(step, newest(&graph), &concluded(Decision::Continue), &[])
            .expect("conclude");

        assert_eq!(reopened, Some(false));
        assert_eq!(
            graph.node(step).expect("node").status,
            NodeStatus::Completed
        );
        assert_eq!(
            kinds(&graph).last(),
            Some(&(EventKind::NodeEvaluated, Some(step.0)))
        );
        assert!(!graph.conclusions_of(step).expect("conclusions")[0].reopened);
    }

    #[test]
    fn an_abandoned_failure_stays_failed_with_why_and_is_explored_past() {
        let (_dir, mut graph, origin, goal, mut inbox) = seeded_with_inbox();
        let step = ran(&mut graph, &mut inbox, origin, goal, true);

        let reopened = graph
            .conclude(step, newest(&graph), &concluded(Decision::Abandoned), &[])
            .expect("conclude");

        assert_eq!(reopened, Some(false));
        assert_eq!(graph.node(step).expect("node").status, NodeStatus::Failed);
        assert_eq!(
            kinds(&graph).last(),
            Some(&(EventKind::NodeEvaluated, Some(step.0)))
        );
        let conclusion = &graph.conclusions_of(step).expect("conclusions")[0];
        assert_eq!(conclusion.decision, Decision::Abandoned);
        assert_eq!(conclusion.resume_commit, None);
        let snapshot = graph.snapshot().expect("snapshot");
        assert_eq!(
            snapshot.conclusion(step).map(|c| c.decision),
            Some(Decision::Abandoned)
        );
    }

    #[test]
    fn a_step_done_is_judged_against_its_goals_in_the_same_record() {
        let (_dir, mut graph, origin, goal, mut inbox) = seeded_with_inbox();
        let step = ran(&mut graph, &mut inbox, origin, goal, false);

        graph
            .conclude(
                step,
                newest(&graph),
                &concluded(Decision::Done),
                &[NewEvaluation {
                    goal,
                    satisfied: true,
                    score: 90,
                    reason: "hello exists".to_owned(),
                }],
            )
            .expect("conclude");

        assert_eq!(graph.evaluations_of(step).expect("evaluations").len(), 1);
        assert_eq!(graph.candidates().expect("candidates").len(), 1);
        assert!(
            graph
                .conclude(origin, newest(&graph), &concluded(Decision::Done), &[])
                .is_err()
        );
    }

    #[test]
    fn a_judgement_of_an_attempt_that_was_started_again_is_not_recorded() {
        let (_dir, mut graph, origin, goal, mut inbox) = seeded_with_inbox();
        let step = ran(&mut graph, &mut inbox, origin, goal, true);
        let failed = newest(&graph);
        graph.replan(step).expect("replan");
        let delivery = lease_ready(&mut inbox, 1, step);
        assert!(graph.claim(step, &delivery.token).expect("claim"));

        let recorded = graph
            .conclude(step, failed, &concluded(Decision::Continue), &[])
            .expect("conclude");

        assert_eq!(recorded, None);
        assert!(graph.conclusions_of(step).expect("conclusions").is_empty());
        assert_eq!(graph.node(step).expect("node").status, NodeStatus::Running);
    }

    #[test]
    fn an_adopted_step_is_not_continued() {
        let (_dir, mut graph, origin, goal, mut inbox) = seeded_with_inbox();
        let step = ran(&mut graph, &mut inbox, origin, goal, false);
        let completed = newest(&graph);
        graph
            .advance_origin(&NewOrigin {
                previous: origin,
                title: "adopted".to_owned(),
                note: String::new(),
                branch: Some("main".to_owned()),
                commit: "def".to_owned(),
                adopted: vec![step],
            })
            .expect("advance");

        let reopened = graph
            .conclude(step, completed, &concluded(Decision::Continue), &[])
            .expect("conclude");

        assert_eq!(reopened, Some(false));
        assert_eq!(
            graph.node(step).expect("node").status,
            NodeStatus::Completed
        );
    }
}
