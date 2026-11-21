//! [`Inbox`] — one agent's view of the event log.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::event::{Event, EventId, EventKind};
use crate::graph::NodeId;
use crate::process::ProcessId;
use crate::store::{self, EPOCH, NOW, StoreError};

/// How long a delivery stays leased without renewal. A process renews the
/// leases it holds on every heartbeat.
pub const LEASE_SECONDS: i64 = 60;

/// An event leased to one process of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The event.
    pub event: Event,
    /// How many times it has been leased, this time included.
    pub attempts: i64,
}

/// The deliveries of one agent. See the [module docs](crate::event).
pub struct Inbox {
    conn: Connection,
    agent: String,
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbox")
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

fn kinds_sql(kinds: &[EventKind]) -> String {
    kinds
        .iter()
        .map(|kind| format!("'{}'", kind.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Inbox {
    /// The inbox of `agent` in the database at `path`.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the database cannot be opened.
    pub fn open(path: &Path, agent: &str) -> Result<Self, StoreError> {
        Ok(Self {
            conn: store::open(path)?,
            agent: agent.to_owned(),
        })
    }

    /// The agent this inbox belongs to.
    #[must_use]
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// Lease the oldest deliverable event of `kinds` to `process`.
    ///
    /// Deliverable: never delivered to this agent, released, or leased by a
    /// process whose lease expired. Events about a node another process of
    /// this agent is handling wait. With `exclusive`, nothing is leased
    /// while any process of this agent holds a lease (single-flight roles).
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn lease(
        &mut self,
        kinds: &[EventKind],
        process: ProcessId,
        exclusive: bool,
    ) -> Result<Option<Delivery>, rusqlite::Error> {
        if kinds.is_empty() {
            return Ok(None);
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if exclusive {
            let busy: bool = tx.query_row(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM deliveries WHERE agent = ?1 \
                     AND status = 'leased' AND leased_until >= {EPOCH})"
                ),
                [&self.agent],
                |r| r.get(0),
            )?;
            if busy {
                return Ok(None);
            }
        }
        let candidate = tx
            .query_row(
                &format!(
                    "SELECT e.id, e.kind, e.node_id, e.created_at, COALESCE(d.attempts, 0) \
                     FROM events e \
                     LEFT JOIN deliveries d ON d.agent = ?1 AND d.event_id = e.id \
                     WHERE e.kind IN ({kinds}) \
                     AND (d.event_id IS NULL OR d.status = 'pending' \
                          OR (d.status = 'leased' AND d.leased_until < {EPOCH})) \
                     AND (e.node_id IS NULL OR NOT EXISTS ( \
                          SELECT 1 FROM deliveries x JOIN events y ON y.id = x.event_id \
                          WHERE x.agent = ?1 AND x.status = 'leased' \
                          AND x.leased_until >= {EPOCH} \
                          AND y.node_id = e.node_id AND x.event_id <> e.id)) \
                     ORDER BY e.id LIMIT 1",
                    kinds = kinds_sql(kinds)
                ),
                [&self.agent],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, kind, node, created_at, attempts)) = candidate else {
            return Ok(None);
        };
        tx.execute(
            &format!(
                "INSERT INTO deliveries (agent, event_id, status, process_id, attempts, leased_until) \
                 VALUES (?1, ?2, 'leased', ?3, 1, {EPOCH} + {LEASE_SECONDS}) \
                 ON CONFLICT (agent, event_id) DO UPDATE SET status = 'leased', \
                 process_id = ?3, attempts = attempts + 1, \
                 leased_until = {EPOCH} + {LEASE_SECONDS}, updated_at = {NOW}"
            ),
            params![self.agent, id, process.0],
        )?;
        tx.commit()?;
        let kind = kind.parse().map_err(|reason: String| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(reason)),
            )
        })?;
        Ok(Some(Delivery {
            event: Event {
                id: EventId(id),
                kind,
                node: node.map(NodeId),
                created_at,
            },
            attempts: attempts + 1,
        }))
    }

    /// The reaction to `event` succeeded.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn ack(&self, event: EventId, note: Option<&str>) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            &format!(
                "UPDATE deliveries SET status = 'done', leased_until = NULL, note = ?3, \
                 updated_at = {NOW} WHERE agent = ?1 AND event_id = ?2"
            ),
            params![self.agent, event.0, note],
        )?;
        Ok(())
    }

    /// Acknowledge every not-yet-handled event of `kinds` up to `through`
    /// as covered by one reaction (roles that consider the whole graph,
    /// like planning, need not react once per event). Events leased by
    /// other live processes are left alone. Returns how many were covered.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn ack_through(
        &self,
        kinds: &[EventKind],
        through: EventId,
        note: &str,
    ) -> Result<usize, rusqlite::Error> {
        if kinds.is_empty() {
            return Ok(0);
        }
        let changed = self.conn.execute(
            &format!(
                "INSERT INTO deliveries (agent, event_id, status, note) \
                 SELECT ?1, e.id, 'done', ?3 FROM events e \
                 LEFT JOIN deliveries d ON d.agent = ?1 AND d.event_id = e.id \
                 WHERE e.kind IN ({kinds}) AND e.id <= ?2 \
                 AND (d.event_id IS NULL OR d.status = 'pending') \
                 ON CONFLICT (agent, event_id) DO UPDATE SET status = 'done', note = ?3, \
                 updated_at = {NOW}",
                kinds = kinds_sql(kinds)
            ),
            params![self.agent, through.0, note],
        )?;
        Ok(changed)
    }

    /// Give `event` back undelivered (the process is stopping); the attempt
    /// does not count.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn release(&self, event: EventId) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            &format!(
                "UPDATE deliveries SET status = 'pending', leased_until = NULL, \
                 attempts = MAX(attempts - 1, 0), updated_at = {NOW} \
                 WHERE agent = ?1 AND event_id = ?2"
            ),
            params![self.agent, event.0],
        )?;
        Ok(())
    }

    /// The reaction to `event` failed: deliver it again later, or give up
    /// after `max_attempts`. Returns whether it will be retried.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn fail(
        &self,
        event: EventId,
        error: &str,
        max_attempts: i64,
    ) -> Result<bool, rusqlite::Error> {
        let attempts: i64 = self.conn.query_row(
            "SELECT attempts FROM deliveries WHERE agent = ?1 AND event_id = ?2",
            params![self.agent, event.0],
            |r| r.get(0),
        )?;
        let retry = attempts < max_attempts;
        self.conn.execute(
            &format!(
                "UPDATE deliveries SET status = ?3, leased_until = NULL, note = ?4, \
                 updated_at = {NOW} WHERE agent = ?1 AND event_id = ?2"
            ),
            params![
                self.agent,
                event.0,
                if retry { "pending" } else { "failed" },
                error
            ],
        )?;
        Ok(retry)
    }

    /// Extend every lease `process` holds in this inbox.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn renew(&self, process: ProcessId) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            &format!(
                "UPDATE deliveries SET leased_until = {EPOCH} + {LEASE_SECONDS} \
                 WHERE agent = ?1 AND process_id = ?2 AND status = 'leased'"
            ),
            params![self.agent, process.0],
        )?;
        Ok(())
    }

    /// Events of `kinds` not yet handled by this agent (pending, never
    /// delivered, or leased).
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn backlog(&self, kinds: &[EventKind]) -> Result<usize, rusqlite::Error> {
        if kinds.is_empty() {
            return Ok(0);
        }
        let count: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM events e \
                 LEFT JOIN deliveries d ON d.agent = ?1 AND d.event_id = e.id \
                 WHERE e.kind IN ({kinds}) \
                 AND (d.event_id IS NULL OR d.status IN ('pending', 'leased'))",
                kinds = kinds_sql(kinds)
            ),
            [&self.agent],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// The delivery status of `event` for this agent, if it was ever
    /// delivered.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn status_of(&self, event: EventId) -> Result<Option<String>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT status FROM deliveries WHERE agent = ?1 AND event_id = ?2",
                params![self.agent, event.0],
                |r| r.get(0),
            )
            .optional()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: std::path::PathBuf,
        graph: Graph,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let mut graph = Graph::open(&db).expect("graph");
        let origin = graph
            .create_origin("o", Some("main"), "abc")
            .expect("origin");
        let goal = graph.add_goal("g", "g", false).expect("goal"); // e1 goal.added
        graph.propose_step("a", "a", &[origin], &[goal]).expect("a"); // e2
        graph.propose_step("b", "b", &[origin], &[goal]).expect("b"); // e3
        Fixture {
            _dir: dir,
            db,
            graph,
        }
    }

    const PROPOSED: &[EventKind] = &[EventKind::NodeProposed];

    #[test]
    fn every_agent_sees_every_event_and_processes_compete() {
        let f = fixture();
        let mut explorer = Inbox::open(&f.db, "explorer").expect("inbox");
        let mut planner_1 = Inbox::open(&f.db, "planner").expect("inbox");
        let mut planner_2 = Inbox::open(&f.db, "planner").expect("inbox");
        let mut other = Inbox::open(&f.db, "other-planner").expect("inbox");

        let goal = explorer
            .lease(&[EventKind::GoalAdded], ProcessId(1), false)
            .expect("lease");
        assert_eq!(goal.map(|d| d.event.id), Some(EventId(1)));

        // Two processes of one agent split the events between them…
        let first = planner_1
            .lease(PROPOSED, ProcessId(2), false)
            .expect("lease")
            .expect("e2");
        let second = planner_2
            .lease(PROPOSED, ProcessId(3), false)
            .expect("lease")
            .expect("e3");
        assert_eq!((first.event.id, second.event.id), (EventId(2), EventId(3)));
        assert!(
            planner_1
                .lease(PROPOSED, ProcessId(2), false)
                .expect("lease")
                .is_none()
        );
        // …while another agent receives all of them again (fan-out).
        assert_eq!(
            other
                .lease(PROPOSED, ProcessId(4), false)
                .expect("lease")
                .map(|d| d.event.id),
            Some(EventId(2))
        );

        planner_1.ack(first.event.id, None).expect("ack");
        assert!(
            planner_2.fail(second.event.id, "boom", 3).expect("fail"),
            "retried"
        );
        let again = planner_1
            .lease(PROPOSED, ProcessId(2), false)
            .expect("lease")
            .expect("e3");
        assert_eq!((again.event.id, again.attempts), (EventId(3), 2));
        assert_eq!(planner_1.backlog(PROPOSED).expect("backlog"), 1);
    }

    #[test]
    fn exclusive_roles_lease_one_at_a_time_and_coalesce() {
        let f = fixture();
        let mut planner_1 = Inbox::open(&f.db, "planner").expect("inbox");
        let mut planner_2 = Inbox::open(&f.db, "planner").expect("inbox");
        let first = planner_1
            .lease(PROPOSED, ProcessId(1), true)
            .expect("lease")
            .expect("e2");
        assert!(
            planner_2
                .lease(PROPOSED, ProcessId(2), true)
                .expect("lease")
                .is_none()
        );
        let through = f.graph.last_event().expect("last").expect("some");
        assert_eq!(
            planner_1
                .ack_through(PROPOSED, through, "covered")
                .expect("cover"),
            1
        );
        planner_1.ack(first.event.id, None).expect("ack");
        assert_eq!(planner_1.backlog(PROPOSED).expect("backlog"), 0);
    }

    #[test]
    fn events_about_one_node_go_to_one_process_at_a_time() {
        let mut f = fixture();
        let a = NodeId(3);
        f.graph.plan_step(a, 1).expect("plan"); // e4 node.planned a, e5 node.ready a
        let kinds = &[EventKind::NodeProposed, EventKind::NodePlanned];
        let mut one = Inbox::open(&f.db, "watcher").expect("inbox");
        let mut two = Inbox::open(&f.db, "watcher").expect("inbox");
        let first = one
            .lease(kinds, ProcessId(1), false)
            .expect("lease")
            .expect("e2");
        assert_eq!(first.event.node, Some(a));
        // e3 is about another node; e4 (about `a`) must wait for e2.
        let second = two
            .lease(kinds, ProcessId(2), false)
            .expect("lease")
            .expect("e3");
        assert_eq!(second.event.id, EventId(3));
        assert!(
            two.lease(kinds, ProcessId(2), false)
                .expect("lease")
                .is_none()
        );
        one.ack(first.event.id, None).expect("ack");
        assert_eq!(
            two.lease(kinds, ProcessId(2), false)
                .expect("lease")
                .map(|d| d.event.id),
            Some(EventId(4))
        );
    }
}
