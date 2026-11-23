//! [`Inbox`] — one agent's view of the event log.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::event::{Event, EventId, EventKind};
use crate::graph::NodeId;
use crate::process::ProcessId;
use crate::role::Role;
use crate::store::{self, EPOCH, NOW, StoreError};

/// How long before a lease expires without renewal. A process renews the
/// lease it holds on every heartbeat.
pub const LEASE_SECONDS: i64 = 60;

/// Proof of one lease, required to renew or settle it. An expired lease
/// still holds its token until the event is leased again, a conflicting
/// event is leased, or the delivery is settled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LeaseToken(String);

impl LeaseToken {
    /// The token as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A deterministic token for tests that create worktrees without an inbox.
    #[cfg(test)]
    pub(crate) fn for_test(value: u128) -> Self {
        Self(format!("{value:032x}"))
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
    /// The token no longer identifies a held lease.
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

/// The delivery row of `event_id = ?2` is still leased under token `?3`. An
/// expired lease remains valid until its token is replaced by a takeover,
/// revoked by a conflicting lease, or settled.
const HELD_LEASE: &str = "status = 'leased' AND lease_token = ?3";

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

/// An event as read from its row.
fn event(
    id: i64,
    kind: &str,
    node: Option<i64>,
    created_at: String,
) -> Result<Event, rusqlite::Error> {
    let kind = kind.parse().map_err(|reason: String| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(reason)),
        )
    })?;
    Ok(Event {
        id: EventId(id),
        kind,
        node: node.map(NodeId),
        created_at,
    })
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

    /// Lease the next deliverable event of `kinds` to `process`: the
    /// oldest, except that `node.ready` events go first, highest current
    /// priority of their step first, so the most promising step is tried
    /// first however long ago it became ready.
    ///
    /// Deliverable: never delivered to this agent, released, or leased by a
    /// process whose lease expired before its `max_attempts`-th attempt.
    /// Every lease counts as an attempt, so a delivery whose holders keep
    /// dying is [abandoned](Self::abandoned) rather than leased forever.
    /// For this agent, an unexpired lease blocks other events about its node.
    /// With `exclusive`, an unexpired lease blocks every event (single-flight
    /// roles). Leasing an event replaces its previous token and revokes any
    /// expired tokens that would conflict, preserving those deliveries for
    /// retry or abandonment.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn lease(
        &mut self,
        kinds: &[EventKind],
        process: ProcessId,
        exclusive: bool,
        max_attempts: i64,
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
                     LEFT JOIN nodes n ON n.id = e.node_id \
                     WHERE e.kind IN ({kinds}) \
                     AND (d.event_id IS NULL OR d.status = 'pending' \
                          OR (d.status = 'leased' AND d.leased_until < {EPOCH} \
                              AND d.attempts < ?2)) \
                     AND (e.node_id IS NULL OR NOT EXISTS ( \
                          SELECT 1 FROM deliveries x JOIN events y ON y.id = x.event_id \
                          WHERE x.agent = ?1 AND x.status = 'leased' \
                          AND x.leased_until >= {EPOCH} \
                          AND y.node_id = e.node_id AND x.event_id <> e.id)) \
                     ORDER BY CASE WHEN e.kind = 'node.ready' THEN n.priority END DESC, \
                     e.id LIMIT 1",
                    kinds = kinds_sql(kinds)
                ),
                params![self.agent, max_attempts],
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
        if exclusive || node.is_some() {
            // The checks above ruled out unexpired conflicts. Revoke the
            // expired ones before granting the lease in this transaction.
            tx.execute(
                "UPDATE deliveries SET lease_token = NULL \
                 WHERE agent = ?1 AND status = 'leased' AND lease_token IS NOT NULL \
                 AND event_id <> ?2 AND (?3 OR EXISTS ( \
                     SELECT 1 FROM events e WHERE e.id = deliveries.event_id AND e.node_id = ?4))",
                params![self.agent, id, exclusive, node],
            )?;
        }
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
        Ok(Some(Delivery {
            event: event(id, &kind, node, created_at)?,
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
                 note = ?4, updated_at = {NOW} WHERE agent = ?1 AND event_id = ?2 AND {HELD_LEASE}"
            ),
            params![self.agent, delivery.event.id.0, delivery.token.0, note],
        )?;
        self.settled(delivery, changed)
    }

    /// While holding `delivery`, acknowledge every not-yet-handled event of
    /// `kinds` up to `through` as covered by its reaction (roles that
    /// consider the whole graph, like planning, need not react once per
    /// event). Leased deliveries are left alone. Returns how many were
    /// covered.
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
                 WHERE agent = ?1 AND event_id = ?2 AND {HELD_LEASE})"
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
                 WHERE agent = ?1 AND event_id = ?2 AND {HELD_LEASE}"
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
                     WHERE agent = ?1 AND event_id = ?2 AND {HELD_LEASE} RETURNING status"
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

    /// The reaction to `delivery` failed and must not be retried, whatever
    /// attempts are left.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn give_up(&self, delivery: &Delivery, error: &str) -> Result<(), InboxError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE deliveries SET status = 'failed', leased_until = NULL, \
                 lease_token = NULL, note = ?4, updated_at = {NOW} \
                 WHERE agent = ?1 AND event_id = ?2 AND {HELD_LEASE}"
            ),
            params![self.agent, delivery.event.id.0, delivery.token.0, error],
        )?;
        self.settled(delivery, changed)
    }

    /// Deliveries of `kinds` whose holder died on their last attempt: the
    /// lease expired after `max_attempts` attempts, so they are never
    /// leased again. Each awaits [`give_up_abandoned`](Self::give_up_abandoned)
    /// once whatever its holder left behind has been cleaned up.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn abandoned(
        &self,
        kinds: &[EventKind],
        max_attempts: i64,
    ) -> Result<Vec<Delivery>, rusqlite::Error> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = self.conn.prepare(&format!(
            // Revoked leases and leases from before tokens existed read as
            // the empty token, which `give_up_abandoned` matches.
            "SELECT e.id, e.kind, e.node_id, e.created_at, d.attempts, \
             COALESCE(d.lease_token, '') \
             FROM deliveries d JOIN events e ON e.id = d.event_id \
             WHERE d.agent = ?1 AND e.kind IN ({kinds}) AND d.status = 'leased' \
             AND d.leased_until < {EPOCH} AND d.attempts >= ?2 ORDER BY e.id",
            kinds = kinds_sql(kinds)
        ))?;
        let rows = statement.query_map(params![self.agent, max_attempts], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (id, kind, node, created_at, attempts, token) = row?;
            Ok(Delivery {
                event: event(id, &kind, node, created_at)?,
                attempts,
                token: LeaseToken(token),
            })
        })
        .collect()
    }

    /// Record an [abandoned](Self::abandoned) delivery as failed, noting
    /// that its holder died. `false` when it is no longer abandoned under
    /// that lease (another process gave it up first).
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn give_up_abandoned(&self, delivery: &Delivery) -> Result<bool, rusqlite::Error> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE deliveries SET status = 'failed', \
                 note = 'its holder, process ' || process_id || ', died during attempt ' \
                     || attempts || ' and the lease expired; giving up', \
                 leased_until = NULL, lease_token = NULL, updated_at = {NOW} \
                 WHERE agent = ?1 AND event_id = ?2 AND status = 'leased' \
                 AND COALESCE(lease_token, '') = ?3 AND leased_until < {EPOCH}"
            ),
            params![self.agent, delivery.event.id.0, delivery.token.0],
        )?;
        Ok(changed == 1)
    }

    /// Extend the lease of `delivery` while its token is current, even
    /// after expiry.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn renew(&self, delivery: &Delivery) -> Result<(), InboxError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE deliveries SET leased_until = {EPOCH} + {LEASE_SECONDS} \
                 WHERE agent = ?1 AND event_id = ?2 AND {HELD_LEASE}"
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

    /// The one agent of `role` that `event` is assigned to, among the
    /// agents sharing the role by weight: the assignment already recorded,
    /// or else `proposed`, recorded now. The first process to route an
    /// event decides, atomically, and every later call — from any process,
    /// whatever weights it loaded, before or after a restart — returns that
    /// same agent, so exactly one weighted agent reacts to the event.
    /// Assign before reacting to an event or acknowledging it as routed
    /// elsewhere.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn assign(
        &self,
        role: Role,
        event: EventId,
        proposed: &str,
    ) -> Result<String, rusqlite::Error> {
        self.conn.query_row(
            "INSERT INTO assignments (role, event_id, agent) VALUES (?1, ?2, ?3) \
             ON CONFLICT (role, event_id) DO UPDATE SET agent = assignments.agent \
             RETURNING agent",
            params![role.as_str(), event.0, proposed],
            |r| r.get(0),
        )
    }

    /// Move `event` of `role` from `from`, an agent that will never handle it
    /// (no longer declared for the role), to `to`, unless another process
    /// already moved it; the agent it is assigned to afterwards.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn reassign(
        &self,
        role: Role,
        event: EventId,
        from: &str,
        to: &str,
    ) -> Result<String, rusqlite::Error> {
        self.conn.execute(
            "UPDATE assignments SET agent = ?4 WHERE role = ?1 AND event_id = ?2 AND agent = ?3",
            params![role.as_str(), event.0, from, to],
        )?;
        self.conn.query_row(
            "SELECT agent FROM assignments WHERE role = ?1 AND event_id = ?2",
            params![role.as_str(), event.0],
            |r| r.get(0),
        )
    }

    /// The agent of `role` that `event` was assigned to, if it was.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn assignment(
        &self,
        role: Role,
        event: EventId,
    ) -> Result<Option<String>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT agent FROM assignments WHERE role = ?1 AND event_id = ?2",
                params![role.as_str(), event.0],
                |r| r.get(0),
            )
            .optional()
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
            .lease(PROPOSED, ProcessId(1), true, 3)
            .expect("lease")
            .expect("e2");

        // The first replica's heartbeat is late and its lease runs out.
        // Nobody has taken it over, so it is still the first replica's.
        expire(&f.db, "planner", stale.event.id);
        first
            .renew(&stale)
            .expect("an expired lease nobody took over renews");

        // Then it stops renewing for good.
        expire(&f.db, "planner", stale.event.id);

        // The second replica reclaims the event under a new token.
        let replacement = second
            .lease(PROPOSED, ProcessId(2), true, 3)
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
                .lease(PROPOSED, ProcessId(1), true, 3)
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
    fn ready_steps_are_leased_by_current_priority() {
        let mut f = fixture();
        let a = NodeId(3);
        let b = NodeId(4);
        // a becomes ready first, at a high priority; b later, at a low one.
        f.graph.plan_step(a, 100).expect("plan a"); // node.planned, node.ready
        f.graph.plan_step(b, 1).expect("plan b");
        let mut inbox = Inbox::open(&f.db, "verifier").expect("inbox");
        let kinds = &[EventKind::NodeReady, EventKind::NodeProposed];
        let mut next = || {
            let delivery = inbox.lease(kinds, ProcessId(1), false, 3).expect("lease")?;
            inbox.ack(&delivery, None).expect("ack");
            Some((delivery.event.kind, delivery.event.node))
        };
        // The planner then ranks a below b: its current priority counts, not
        // the one it had when it became ready, nor which became ready first.
        f.graph.plan_step(a, 0).expect("lower a");
        let order: Vec<_> = std::iter::from_fn(&mut next).collect();
        assert_eq!(
            order,
            vec![
                (EventKind::NodeReady, Some(b)),
                (EventKind::NodeReady, Some(a)),
                // Other kinds after every ready step, oldest first.
                (EventKind::NodeProposed, Some(a)),
                (EventKind::NodeProposed, Some(b)),
            ]
        );
    }

    #[test]
    fn expired_leases_count_toward_the_limit() {
        let f = fixture();
        let mut inbox = Inbox::open(&f.db, "planner").expect("inbox");
        for attempt in 1..=3 {
            let delivery = inbox
                .lease(PROPOSED, ProcessId(attempt), true, 3)
                .expect("lease")
                .expect("e2");
            assert_eq!(
                (delivery.event.id, delivery.attempts),
                (EventId(2), attempt)
            );
            assert!(inbox.abandoned(PROPOSED, 3).expect("abandoned").is_empty());
            // The holder dies without settling.
            expire(&f.db, "planner", delivery.event.id);
        }
        let abandoned = inbox.abandoned(PROPOSED, 3).expect("abandoned");
        assert_eq!(
            abandoned
                .iter()
                .map(|d| (d.event.id, d.attempts))
                .collect::<Vec<_>>(),
            vec![(EventId(2), 3)]
        );
        // A higher limit would still deliver it again.
        assert_eq!(inbox.abandoned(PROPOSED, 4).expect("abandoned"), Vec::new());
    }

    #[test]
    fn a_lease_from_before_tokens_is_given_up_too() {
        let f = fixture();
        let inbox = Inbox::open(&f.db, "planner").expect("inbox");
        store::open(&f.db)
            .expect("db")
            .execute(
                "INSERT INTO deliveries (agent, event_id, status, process_id, attempts, \
                 leased_until, lease_token) VALUES ('planner', 2, 'leased', 7, 3, 0, NULL)",
                [],
            )
            .expect("old lease");
        let abandoned = inbox.abandoned(PROPOSED, 3).expect("abandoned");
        assert_eq!(abandoned.len(), 1);
        assert!(inbox.give_up_abandoned(&abandoned[0]).expect("give up"));
        assert!(inbox.abandoned(PROPOSED, 3).expect("abandoned").is_empty());
    }

    #[test]
    fn an_exhausted_delivery_is_not_leased_again() {
        let f = fixture();
        let mut inbox = Inbox::open(&f.db, "planner").expect("inbox");
        let mut other = Inbox::open(&f.db, "planner").expect("inbox");
        let first = inbox
            .lease(PROPOSED, ProcessId(1), true, 2)
            .expect("lease")
            .expect("e2");
        expire(&f.db, "planner", first.event.id);
        let last = inbox
            .lease(PROPOSED, ProcessId(2), true, 2)
            .expect("lease")
            .expect("e2 again");
        assert_eq!((last.event.id, last.attempts), (EventId(2), 2));
        expire(&f.db, "planner", last.event.id);
        let abandoned = inbox.abandoned(PROPOSED, 2).expect("abandoned");
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].token, last.token);

        // Its last holder died: e2 is skipped, and the next event is leased.
        let next = inbox
            .lease(PROPOSED, ProcessId(3), true, 2)
            .expect("lease")
            .expect("e3");
        assert_eq!(next.event.id, EventId(3));
        assert_lost(other.renew(&last), last.event.id);
        assert!(!other.give_up_abandoned(&abandoned[0]).expect("stale token"));
        inbox.ack(&next, None).expect("ack");
        assert!(
            inbox
                .lease(PROPOSED, ProcessId(3), true, 2)
                .expect("lease")
                .is_none()
        );

        let abandoned = inbox.abandoned(PROPOSED, 2).expect("abandoned");
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].event.id, last.event.id);
        assert!(inbox.give_up_abandoned(&abandoned[0]).expect("give up"));
        assert!(
            !other.give_up_abandoned(&abandoned[0]).expect("give up"),
            "given up once"
        );
        let (status, process, attempts, _, _, note) = {
            let conn = store::open(&f.db).expect("db");
            conn.query_row(
                "SELECT status, process_id, attempts, leased_until, lease_token, note \
                 FROM deliveries WHERE agent = 'planner' AND event_id = 2",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .expect("row")
        };
        assert_eq!((status.as_str(), process, attempts), ("failed", 2, 2));
        let note = note.expect("note");
        assert!(note.contains("process 2, died"), "{note}");
        assert!(inbox.abandoned(PROPOSED, 2).expect("abandoned").is_empty());
        assert!(
            other
                .lease(PROPOSED, ProcessId(4), true, 2)
                .expect("lease")
                .is_none(),
            "a failed delivery is never leased again"
        );
        assert_eq!(other.backlog(PROPOSED).expect("backlog"), 0);
    }

    #[test]
    fn a_held_delivery_can_be_given_up_with_attempts_left() {
        let f = fixture();
        let mut inbox = Inbox::open(&f.db, "planner").expect("inbox");
        let delivery = inbox
            .lease(PROPOSED, ProcessId(1), true, 3)
            .expect("lease")
            .expect("e2");
        inbox.give_up(&delivery, "graph broke").expect("give up");
        assert_eq!(
            inbox.status_of(delivery.event.id).expect("status"),
            Some("failed".into())
        );
        assert_lost(inbox.give_up(&delivery, "again"), delivery.event.id);
        let next = inbox
            .lease(PROPOSED, ProcessId(1), true, 3)
            .expect("lease")
            .expect("e3");
        assert_eq!(next.event.id, EventId(3));
    }

    #[test]
    fn every_agent_sees_every_event_and_processes_compete() {
        let f = fixture();
        let mut explorer = Inbox::open(&f.db, "explorer").expect("inbox");
        let mut planner_1 = Inbox::open(&f.db, "planner").expect("inbox");
        let mut planner_2 = Inbox::open(&f.db, "planner").expect("inbox");
        let mut other = Inbox::open(&f.db, "other-planner").expect("inbox");

        let goal = explorer
            .lease(&[EventKind::GoalAdded], ProcessId(1), false, 3)
            .expect("lease");
        assert_eq!(goal.map(|d| d.event.id), Some(EventId(1)));

        // Two processes of one agent split the events between them…
        let first = planner_1
            .lease(PROPOSED, ProcessId(2), false, 3)
            .expect("lease")
            .expect("e2");
        let second = planner_2
            .lease(PROPOSED, ProcessId(3), false, 3)
            .expect("lease")
            .expect("e3");
        assert_eq!((first.event.id, second.event.id), (EventId(2), EventId(3)));
        assert!(
            planner_1
                .lease(PROPOSED, ProcessId(2), false, 3)
                .expect("lease")
                .is_none()
        );
        // …while another agent receives all of them again (fan-out).
        assert_eq!(
            other
                .lease(PROPOSED, ProcessId(4), false, 3)
                .expect("lease")
                .map(|d| d.event.id),
            Some(EventId(2))
        );

        planner_1.ack(&first, None).expect("ack");
        assert!(planner_2.fail(&second, "boom", 3).expect("fail"), "retried");
        let again = planner_1
            .lease(PROPOSED, ProcessId(2), false, 3)
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
            .lease(PROPOSED, ProcessId(1), true, 3)
            .expect("lease")
            .expect("e2");
        assert!(
            planner_2
                .lease(PROPOSED, ProcessId(2), true, 3)
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
    fn exclusive_lease_revokes_an_expired_lease_on_another_node() {
        let mut f = fixture();
        let a = NodeId(3);
        let b = NodeId(4);
        let ready = &[EventKind::NodeReady];
        f.graph.plan_step(a, 1).expect("plan a");
        let mut first = Inbox::open(&f.db, "planner").expect("inbox");
        let mut second = Inbox::open(&f.db, "planner").expect("inbox");
        let stale = first
            .lease(ready, ProcessId(1), true, 3)
            .expect("lease")
            .expect("a is ready");
        assert_eq!(stale.event.node, Some(a));
        expire(&f.db, "planner", stale.event.id);

        f.graph.plan_step(b, 2).expect("plan b at higher priority");
        let next = second
            .lease(ready, ProcessId(2), true, 3)
            .expect("lease")
            .expect("b is ready");
        assert_eq!(next.event.node, Some(b));
        assert_lost(first.renew(&stale), stale.event.id);
        second.renew(&next).expect("renew b");
        second.ack(&next, None).expect("ack b");

        let retry = second
            .lease(ready, ProcessId(2), true, 3)
            .expect("lease")
            .expect("a is retried");
        assert_eq!((retry.event.id, retry.attempts), (stale.event.id, 2));
        assert_ne!(retry.token, stale.token);
    }

    #[test]
    fn the_first_assignment_of_an_event_stands() {
        let f = fixture();
        let event = EventId(1);
        // Processes of two agents propose different recipients at once,
        // as if they had loaded different weights.
        let owners: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let db = f.db.clone();
                    scope.spawn(move || {
                        let (agent, proposed) = if i % 2 == 0 {
                            ("explorer", "contrarian")
                        } else {
                            ("contrarian", "explorer")
                        };
                        Inbox::open(&db, agent)
                            .expect("inbox")
                            .assign(Role::Exploration, event, proposed)
                            .expect("assign")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread"))
                .collect()
        });
        let first = owners[0].clone();
        assert!(owners.iter().all(|o| *o == first), "{owners:?}");

        // An assignment to an agent that is gone moves once, to whichever
        // recipient asks first.
        let gone = Inbox::open(&f.db, "explorer").expect("inbox");
        gone.assign(Role::Exploration, EventId(2), "removed")
            .expect("assign");
        assert_eq!(
            gone.reassign(Role::Exploration, EventId(2), "removed", "explorer")
                .expect("reassign"),
            "explorer"
        );
        assert_eq!(
            gone.reassign(Role::Exploration, EventId(2), "removed", "contrarian")
                .expect("reassign"),
            "explorer"
        );

        // After a restart, with yet other weights, the record still stands.
        let reopened = Inbox::open(&f.db, "explorer").expect("inbox");
        let other = if first == "explorer" {
            "contrarian"
        } else {
            "explorer"
        };
        assert_eq!(
            reopened
                .assign(Role::Exploration, event, other)
                .expect("assign"),
            first
        );
        assert_eq!(
            reopened
                .assignment(Role::Exploration, event)
                .expect("assignment"),
            Some(first)
        );
        // Each role assigns its events on its own.
        assert_eq!(
            reopened
                .assignment(Role::Planning, event)
                .expect("assignment"),
            None
        );
        assert_eq!(
            reopened
                .assign(Role::Planning, event, "planner")
                .expect("assign"),
            "planner"
        );
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
            .lease(kinds, ProcessId(1), false, 3)
            .expect("lease")
            .expect("e2");
        assert_eq!(first.event.node, Some(a));
        // e3 is about another node; e4 (about `a`) must wait for e2.
        let second = two
            .lease(kinds, ProcessId(2), false, 3)
            .expect("lease")
            .expect("e3");
        assert_eq!(second.event.id, EventId(3));
        assert!(
            two.lease(kinds, ProcessId(2), false, 3)
                .expect("lease")
                .is_none()
        );
        one.ack(&first, None).expect("ack");
        assert_eq!(
            two.lease(kinds, ProcessId(2), false, 3)
                .expect("lease")
                .map(|d| d.event.id),
            Some(EventId(4))
        );
        two.renew(&second)
            .expect("renew the lease on the other node");
    }

    #[test]
    fn leasing_another_event_about_a_node_revokes_its_expired_lease() {
        let mut f = fixture();
        let a = NodeId(3);
        f.graph.plan_step(a, 1).expect("plan a");
        let mut first = Inbox::open(&f.db, "watcher").expect("inbox");
        let mut second = Inbox::open(&f.db, "watcher").expect("inbox");
        let stale = first
            .lease(&[EventKind::NodeReady], ProcessId(1), false, 3)
            .expect("lease")
            .expect("a is ready");
        assert!(f.graph.claim(a, &stale.token).expect("record node.started"));
        expire(&f.db, "watcher", stale.event.id);
        let next = second
            .lease(&[EventKind::NodeStarted], ProcessId(2), false, 3)
            .expect("lease")
            .expect("a has started");
        assert_eq!(stale.event.node, Some(a));
        assert_eq!(next.event.node, stale.event.node);
        assert_ne!(next.event.id, stale.event.id);
        assert_lost(first.renew(&stale), stale.event.id);
        assert_lost(first.ack(&stale, None), stale.event.id);
        second.renew(&next).expect("renew node.started");
    }
}
