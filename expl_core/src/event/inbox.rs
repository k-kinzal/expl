//! [`Inbox`] — one agent's view of the event log.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::event::{Event, EventId, EventKind};
use crate::graph::NodeId;
use crate::process::ProcessId;
use crate::store::{self, EPOCH, NOW, StoreError};

/// How long a delivery stays leased without renewal. A process renews the
/// lease it holds on every heartbeat.
pub const LEASE_SECONDS: i64 = 60;

/// Proof of one lease: unique to it, so a process whose lease expired and
/// was taken over cannot touch the replacement.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LeaseToken(String);

impl LeaseToken {
    /// The token as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LeaseToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An event leased to one process of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The event.
    pub event: Event,
    /// How many times it has been leased, this time included.
    pub attempts: i64,
    /// This lease's token, required to renew or settle it.
    pub token: LeaseToken,
}

/// An operation on a held lease could not be performed.
#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    /// The lease expired or was taken over; nothing was changed.
    #[error("agent `{agent}` no longer holds the lease on event {event}")]
    LeaseLost {
        /// The agent whose inbox it is.
        agent: String,
        /// The event whose lease was lost.
        event: EventId,
    },
    /// SQLite failure.
    #[error("inbox database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// The delivery row of `event_id = ?2` is held under token `?3`: still
/// leased with that token, and not expired.
fn held() -> String {
    format!("status = 'leased' AND lease_token = ?3 AND leased_until >= {EPOCH}")
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
        let token: String = tx.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
        tx.execute(
            &format!(
                "INSERT INTO deliveries \
                 (agent, event_id, status, process_id, attempts, leased_until, lease_token) \
                 VALUES (?1, ?2, 'leased', ?3, 1, {EPOCH} + {LEASE_SECONDS}, ?4) \
                 ON CONFLICT (agent, event_id) DO UPDATE SET status = 'leased', \
                 process_id = ?3, attempts = attempts + 1, \
                 leased_until = {EPOCH} + {LEASE_SECONDS}, lease_token = ?4, updated_at = {NOW}"
            ),
            params![self.agent, id, process.0, token],
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
            token: LeaseToken(token),
        }))
    }

    fn lost(&self, delivery: &Delivery) -> InboxError {
        InboxError::LeaseLost {
            agent: self.agent.clone(),
            event: delivery.event.id,
        }
    }

    /// `Ok` when `changed` rows prove the lease of `delivery` was held.
    fn settled(&self, delivery: &Delivery, changed: usize) -> Result<(), InboxError> {
        if changed == 0 {
            Err(self.lost(delivery))
        } else {
            Ok(())
        }
    }

    /// The reaction to `delivery` succeeded.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn ack(&self, delivery: &Delivery, note: Option<&str>) -> Result<(), InboxError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE deliveries SET status = 'done', leased_until = NULL, lease_token = NULL, \
                 note = ?4, updated_at = {NOW} WHERE agent = ?1 AND event_id = ?2 AND {held}",
                held = held()
            ),
            params![self.agent, delivery.event.id.0, delivery.token.0, note],
        )?;
        self.settled(delivery, changed)
    }

    /// While holding `delivery`, acknowledge every not-yet-handled event of
    /// `kinds` up to `through` as covered by its reaction (roles that
    /// consider the whole graph, like planning, need not react once per
    /// event). Events leased by other processes are left alone. Returns how
    /// many were covered.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held (nothing
    /// is covered); otherwise SQLite failure.
    pub fn ack_through(
        &mut self,
        delivery: &Delivery,
        kinds: &[EventKind],
        through: EventId,
        note: &str,
    ) -> Result<usize, InboxError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let holding: bool = tx.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM deliveries \
                 WHERE agent = ?1 AND event_id = ?2 AND {held})",
                held = held()
            ),
            params![self.agent, delivery.event.id.0, delivery.token.0],
            |r| r.get(0),
        )?;
        if !holding {
            return Err(InboxError::LeaseLost {
                agent: self.agent.clone(),
                event: delivery.event.id,
            });
        }
        let changed = if kinds.is_empty() {
            0
        } else {
            tx.execute(
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
            )?
        };
        tx.commit()?;
        Ok(changed)
    }

    /// Give `delivery` back undelivered (the process is stopping); the
    /// attempt does not count.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn release(&self, delivery: &Delivery) -> Result<(), InboxError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE deliveries SET status = 'pending', leased_until = NULL, \
                 lease_token = NULL, attempts = MAX(attempts - 1, 0), updated_at = {NOW} \
                 WHERE agent = ?1 AND event_id = ?2 AND {held}",
                held = held()
            ),
            params![self.agent, delivery.event.id.0, delivery.token.0],
        )?;
        self.settled(delivery, changed)
    }

    /// The reaction to `delivery` failed: deliver it again later, or give
    /// up after `max_attempts`. Returns whether it will be retried.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn fail(
        &self,
        delivery: &Delivery,
        error: &str,
        max_attempts: i64,
    ) -> Result<bool, InboxError> {
        let status: Option<String> = self
            .conn
            .query_row(
                &format!(
                    "UPDATE deliveries SET \
                     status = CASE WHEN attempts < ?5 THEN 'pending' ELSE 'failed' END, \
                     leased_until = NULL, lease_token = NULL, note = ?4, updated_at = {NOW} \
                     WHERE agent = ?1 AND event_id = ?2 AND {held} RETURNING status",
                    held = held()
                ),
                params![
                    self.agent,
                    delivery.event.id.0,
                    delivery.token.0,
                    error,
                    max_attempts
                ],
                |r| r.get(0),
            )
            .optional()?;
        match status {
            Some(status) => Ok(status == "pending"),
            None => Err(self.lost(delivery)),
        }
    }

    /// Extend the lease of `delivery`. An expired lease cannot be renewed,
    /// even if no other process has taken it over yet.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn renew(&self, delivery: &Delivery) -> Result<(), InboxError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE deliveries SET leased_until = {EPOCH} + {LEASE_SECONDS} \
                 WHERE agent = ?1 AND event_id = ?2 AND {held}",
                held = held()
            ),
            params![self.agent, delivery.event.id.0, delivery.token.0],
        )?;
        self.settled(delivery, changed)
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
        let goal = graph.add_goal("g", "g", false, &[]).expect("goal"); // e1 goal.added
        graph.propose_step("a", "a", &[origin], &[goal]).expect("a"); // e2
        graph.propose_step("b", "b", &[origin], &[goal]).expect("b"); // e3
        Fixture {
            _dir: dir,
            db,
            graph,
        }
    }

    const PROPOSED: &[EventKind] = &[EventKind::NodeProposed];

    /// Let the lease on `event` run out, as if its holder stopped renewing.
    fn expire(db: &Path, agent: &str, event: EventId) {
        let conn = store::open(db).expect("db");
        let changed = conn
            .execute(
                &format!(
                    "UPDATE deliveries SET leased_until = {EPOCH} - 1 \
                     WHERE agent = ?1 AND event_id = ?2 AND status = 'leased'"
                ),
                params![agent, event.0],
            )
            .expect("expire");
        assert_eq!(changed, 1);
    }

    /// Everything stored about the delivery of `event`.
    fn row(
        db: &Path,
        agent: &str,
        event: EventId,
    ) -> (String, i64, i64, i64, String, Option<String>) {
        store::open(db)
            .expect("db")
            .query_row(
                "SELECT status, process_id, attempts, leased_until, lease_token, note \
                 FROM deliveries WHERE agent = ?1 AND event_id = ?2",
                params![agent, event.0],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .expect("row")
    }

    fn assert_lost<T: std::fmt::Debug>(result: Result<T, InboxError>, event: EventId) {
        match result {
            Err(InboxError::LeaseLost { event: lost, .. }) => assert_eq!(lost, event),
            other => panic!("expected a lost lease on {event}, got {other:?}"),
        }
    }

    #[test]
    fn a_reclaimed_lease_ignores_its_former_holder() {
        let f = fixture();
        let mut first = Inbox::open(&f.db, "planner").expect("inbox");
        let mut second = Inbox::open(&f.db, "planner").expect("inbox");
        let stale = first
            .lease(PROPOSED, ProcessId(1), true)
            .expect("lease")
            .expect("e2");

        // The first replica stops renewing; its lease runs out, and an
        // expired lease cannot be renewed back to life.
        expire(&f.db, "planner", stale.event.id);
        assert_lost(first.renew(&stale), stale.event.id);

        // The second replica reclaims the event under a new token.
        let replacement = second
            .lease(PROPOSED, ProcessId(2), true)
            .expect("lease")
            .expect("e2 again");
        assert_eq!(replacement.event.id, stale.event.id);
        assert_eq!(replacement.attempts, 2);
        assert_ne!(replacement.token, stale.token);
        let held = row(&f.db, "planner", replacement.event.id);
        assert_eq!(
            (held.0.as_str(), held.1, held.2, held.4.as_str()),
            ("leased", 2, 2, replacement.token.as_str())
        );

        // Every delayed operation of the first replica is refused and
        // leaves the replacement lease exactly as it was.
        let through = f.graph.last_event().expect("last").expect("some");
        assert_lost(first.renew(&stale), stale.event.id);
        assert_lost(
            first.ack_through(&stale, PROPOSED, through, "covered"),
            stale.event.id,
        );
        assert_lost(first.ack(&stale, Some("done")), stale.event.id);
        assert_lost(first.fail(&stale, "boom", 3), stale.event.id);
        assert_lost(first.release(&stale), stale.event.id);
        assert_eq!(row(&f.db, "planner", replacement.event.id), held);
        assert_eq!(first.status_of(EventId(3)).expect("status"), None);
        assert!(
            first
                .lease(PROPOSED, ProcessId(1), true)
                .expect("lease")
                .is_none(),
            "the replacement still holds the single-flight lease"
        );

        // The replacement goes on undisturbed, and once it is settled its
        // token is spent too.
        second.renew(&replacement).expect("renew");
        assert_eq!(
            second
                .ack_through(&replacement, PROPOSED, through, "covered")
                .expect("cover"),
            1
        );
        second.ack(&replacement, Some("planned")).expect("ack");
        assert_eq!(
            second.status_of(replacement.event.id).expect("status"),
            Some("done".into())
        );
        assert_lost(second.ack(&replacement, None), replacement.event.id);
        assert_lost(first.release(&stale), stale.event.id);
        assert_eq!(second.backlog(PROPOSED).expect("backlog"), 0);
    }

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

        planner_1.ack(&first, None).expect("ack");
        assert!(planner_2.fail(&second, "boom", 3).expect("fail"), "retried");
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
                .ack_through(&first, PROPOSED, through, "covered")
                .expect("cover"),
            1
        );
        planner_1.ack(&first, None).expect("ack");
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
        one.ack(&first, None).expect("ack");
        assert_eq!(
            two.lease(kinds, ProcessId(2), false)
                .expect("lease")
                .map(|d| d.event.id),
            Some(EventId(4))
        );
    }
}
