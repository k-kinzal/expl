//! [`Inbox`] — one consumer's view of an explorer's event log.
//!
//! Each consumer has its own inbox. A delivery group records one eligible
//! recipient per event before issuing a delivery. Workers of a consumer
//! compete for its deliveries, with one lease at a time for events about
//! the same subject.
//!
//! A delivery is a lease: it is acknowledged only after the reaction
//! succeeds. If the worker dies, the lease expires and the event is
//! delivered again. Every lease has its own [`LeaseToken`]: after expiry,
//! taking over the same event replaces it, and leasing a conflicting event
//! revokes it. Renewing, acknowledging, failing, releasing, or coalescing
//! a delivery requires the current token, which stays valid after expiry
//! until it is replaced, revoked, or settled ([`InboxError::LeaseLost`]).

use std::fmt;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, named_params, params};

use crate::event::{Disposition, EPOCH, EventId, Lease as Held, NOW, Settlement};
use crate::registration::{Registration, Route};
use crate::store::{self, StoreError};

/// How long before a lease expires without renewal. A worker renews the
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

    /// The token `value`, as it was handed to the command reacting under
    /// the lease. A token nobody was granted settles nothing.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// One immutable fact in an explorer's event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Position in the log.
    pub id: EventId,
    /// What happened, in the application's vocabulary.
    pub kind: String,
    /// What it happened to, if anything: a row of the application's own.
    pub subject: Option<i64>,
    /// When (UTC, RFC 3339).
    pub created_at: String,
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.subject {
            Some(subject) => write!(f, "{} {} #{subject}", self.id, self.kind),
            None => write!(f, "{} {}", self.id, self.kind),
        }
    }
}

impl fmt::Display for LeaseToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An event leased to one worker of a consumer.
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
    #[error("consumer `{consumer}` no longer holds the lease on event {event}")]
    LeaseLost {
        /// The consumer whose inbox it is.
        consumer: String,
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

// Legacy weighted routing acknowledged deliveries on every peer. Those
// acknowledgments do not count as reactions when an event is reassigned.
const ROUTED_AWAY_NOTE: &str = "ignored: routed to `*`";

fn routed_away() -> String {
    format!("(:group IS NOT NULL AND d.status = 'done' AND d.note GLOB '{ROUTED_AWAY_NOTE}')")
}

/// A routed event is eligible when unassigned, already ours, or stranded
/// with a former recipient that neither settled it nor still holds it.
/// With a NULL group, deliveries are independent of assignments.
/// The SQLite columns `role` and `agent` hold group and consumer names, so
/// existing workers and assignment records can be read.
fn route_filter() -> String {
    format!(
        "(a.agent IS NULL OR a.agent = :agent OR (
             a.agent NOT IN (SELECT value FROM json_each(:members, '$.\"' || e.kind || '\"')) AND NOT EXISTS (
                 SELECT 1 FROM deliveries previous
                 WHERE previous.event_id = e.id AND previous.agent = a.agent
                 AND (previous.status = 'failed' OR
                      (previous.status = 'done' AND
                       COALESCE(previous.note, '') NOT GLOB '{ROUTED_AWAY_NOTE}') OR
                      (previous.status = 'leased' AND previous.leased_until >= {EPOCH})))))"
    )
}

/// What asking for the next delivery came to.
#[derive(Debug)]
pub enum Lease<T> {
    /// Nothing is deliverable to this consumer now.
    Idle,
    /// An event is deliverable, but there is no room to react to it.
    AtCapacity,
    /// The delivery, with the capacity taken for it.
    Granted(Delivery, T),
}

/// The deliveries of one consumer. See the [module docs](self).
pub struct Inbox {
    conn: Connection,
    consumer: String,
}

impl fmt::Debug for Inbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inbox")
            .field("consumer", &self.consumer)
            .finish_non_exhaustive()
    }
}

fn kinds_sql<K: AsRef<str>>(kinds: &[K]) -> String {
    kinds
        .iter()
        .map(|kind| format!("'{}'", kind.as_ref().replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Inbox {
    /// The inbox of `consumer` in the database at `path`.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the database cannot be opened.
    pub fn open(path: &Path, consumer: &str) -> Result<Self, StoreError> {
        Ok(Self {
            conn: store::open(path)?,
            consumer: consumer.to_owned(),
        })
    }

    /// The inbox of `consumer` over `conn`, a connection the application
    /// opened (and migrated) itself.
    #[must_use]
    pub fn over(conn: Connection, consumer: &str) -> Self {
        Self {
            conn,
            consumer: consumer.to_owned(),
        }
    }

    /// The consumer this inbox belongs to.
    #[must_use]
    pub fn consumer(&self) -> &str {
        &self.consumer
    }

    /// The W3C trace context (`traceparent`, `tracestate`) recorded with
    /// the event by its producer, if it had one. A telemetry lookup must
    /// never affect delivery of the event itself, so failures read as none.
    #[must_use]
    pub fn trace_context(&self, id: EventId) -> Option<(String, Option<String>)> {
        let (parent, state) = self
            .conn
            .query_row(
                "SELECT traceparent, tracestate FROM events WHERE id = ?1",
                [id.0],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get(1)?)),
            )
            .ok()?;
        Some((parent?, state))
    }

    /// Lease the next deliverable event of `kinds` to the worker `process`:
    /// the one with the highest current
    /// [priority](crate::event::EventLog::prioritize), and the oldest among
    /// equals, so the most promising work is tried first however long ago
    /// it was recorded.
    ///
    /// Deliverable: never delivered to this consumer, released, or leased by a
    /// worker whose lease expired before its `max_attempts`-th attempt.
    /// Every lease counts as an attempt, so a delivery whose holders keep
    /// dying is [abandoned](Self::abandoned) rather than leased forever.
    /// For this consumer, an unexpired lease blocks other events about its subject.
    /// With `exclusive`, an unexpired lease blocks every event (single-flight
    /// handlers). Leasing an event replaces its previous token and revokes any
    /// expired tokens that would conflict, preserving those deliveries for
    /// retry or abandonment.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn lease<K: AsRef<str>>(
        &mut self,
        kinds: &[K],
        process: i64,
        exclusive: bool,
        max_attempts: i64,
    ) -> Result<Option<Delivery>, rusqlite::Error> {
        match self.lease_with::<_, (), rusqlite::Error>(
            kinds,
            process,
            exclusive,
            max_attempts,
            None,
            || Ok(Some(())),
        )? {
            Lease::Granted(delivery, ()) => Ok(Some(delivery)),
            Lease::Idle | Lease::AtCapacity => Ok(None),
        }
    }

    /// [Lease](Self::lease) with a delivery group's `route` and a capacity
    /// check: find or record the event's recipient before taking a
    /// delivery, try to acquire capacity only for work that belongs to this
    /// consumer, and leave the event unleased when `try_capacity` has none.
    ///
    /// # Errors
    ///
    /// SQLite failure, or the error of `try_capacity`.
    pub fn lease_with<K: AsRef<str>, T, E: From<rusqlite::Error>>(
        &mut self,
        kinds: &[K],
        process: i64,
        exclusive: bool,
        max_attempts: i64,
        route: Option<&Route<'_>>,
        try_capacity: impl FnOnce() -> Result<Option<T>, E>,
    ) -> Result<Lease<T>, E> {
        if kinds.is_empty() {
            return Ok(Lease::Idle);
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
                [&self.consumer],
                |r| r.get(0),
            )?;
            if busy {
                return Ok(Lease::AtCapacity);
            }
        }
        let group = route.map(Route::group);
        let members = route.map(Route::eligible_owners);
        let limit = if route.is_none() { "LIMIT 1" } else { "" };
        let mut statement = tx.prepare(&format!(
            "SELECT e.id, e.kind, e.subject, e.created_at, \
                    CASE WHEN {routed_away} THEN 0 ELSE COALESCE(d.attempts, 0) END, a.agent \
                     FROM events e \
                     LEFT JOIN deliveries d ON d.agent = :agent AND d.event_id = e.id \
                     LEFT JOIN assignments a ON a.role = :group AND a.event_id = e.id \
                     WHERE e.kind IN ({kinds}) \
                     AND {route_filter} \
                     AND (d.event_id IS NULL OR d.status = 'pending' OR {routed_away} \
                          OR (d.status = 'leased' AND d.leased_until < {EPOCH} \
                              AND d.attempts < :max_attempts)) \
                     AND (e.subject IS NULL OR NOT EXISTS ( \
                          SELECT 1 FROM deliveries x JOIN events y ON y.id = x.event_id \
                          WHERE x.agent = :agent AND x.status = 'leased' \
                          AND x.leased_until >= {EPOCH} \
                          AND y.subject = e.subject AND x.event_id <> e.id)) \
                     ORDER BY e.priority DESC, e.id {limit}",
            kinds = kinds_sql(kinds),
            route_filter = route_filter(),
            routed_away = routed_away()
        ))?;
        let candidates = statement.query_map(named_params! {
            ":agent": self.consumer, ":max_attempts": max_attempts, ":group": group, ":members": members,
        }, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut assignments = Vec::new();
        let mut candidate = None;
        for next in candidates {
            let next = next?;
            let (id, kind, _, _, _, owner) = &next;
            if let Some(route) = route.filter(|_| owner.as_deref() != Some(&self.consumer)) {
                let chosen = route.select(kind, EventId(*id));
                assignments.push((*id, chosen, owner.clone()));
                if chosen != self.consumer {
                    continue;
                }
            }
            candidate = Some(next);
            break;
        }
        drop(statement);
        if !assignments.is_empty() {
            let mut assign = tx.prepare_cached(
                "INSERT INTO assignments (role, event_id, agent) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (role, event_id) DO UPDATE SET agent = excluded.agent",
            )?;
            let mut revoke = tx.prepare_cached(
                "UPDATE deliveries SET lease_token = NULL \
                 WHERE event_id = ?1 AND agent = ?2 AND status = 'leased'",
            )?;
            for (id, agent, former) in assignments {
                if let Some(former) = former {
                    // A former owner's lease is expired per the route filter.
                    // It cannot renew after the event moves to another agent.
                    revoke.execute(params![id, former])?;
                }
                assign.execute(params![group, id, agent])?;
            }
        }
        let Some((id, kind, subject, created_at, attempts, _)) = candidate else {
            tx.commit()?;
            return Ok(Lease::Idle);
        };
        let Some(capacity) = try_capacity()? else {
            tx.commit()?;
            return Ok(Lease::AtCapacity);
        };
        if exclusive || subject.is_some() {
            // The checks above ruled out unexpired conflicts. Revoke the
            // expired ones before granting the lease in this transaction.
            tx.execute(
                "UPDATE deliveries SET lease_token = NULL \
                 WHERE agent = ?1 AND status = 'leased' AND lease_token IS NOT NULL \
                 AND event_id <> ?2 AND (?3 OR EXISTS ( \
                     SELECT 1 FROM events e WHERE e.id = deliveries.event_id AND e.subject = ?4))",
                params![self.consumer, id, exclusive, subject],
            )?;
        }
        let token = Held::grant(
            &tx,
            &self.consumer,
            id,
            process,
            attempts + 1,
            LEASE_SECONDS,
        )?;
        tx.commit()?;
        Ok(Lease::Granted(
            Delivery {
                event: Event {
                    id: EventId(id),
                    kind,
                    subject,
                    created_at,
                },
                attempts: attempts + 1,
                token: LeaseToken(token),
            },
            capacity,
        ))
    }

    fn lost(&self, delivery: &Delivery) -> InboxError {
        InboxError::LeaseLost {
            consumer: self.consumer.clone(),
            event: delivery.event.id,
        }
    }

    fn lease_identity<'a>(&'a self, delivery: &'a Delivery) -> Held<'a> {
        Held {
            consumer: &self.consumer,
            event: delivery.event.id.0,
            token: delivery.token.as_str(),
        }
    }

    fn settle(
        &self,
        delivery: &Delivery,
        disposition: Disposition<'_>,
    ) -> Result<bool, InboxError> {
        match self
            .lease_identity(delivery)
            .settle(&self.conn, disposition)?
        {
            Settlement::Lost => Err(self.lost(delivery)),
            Settlement::Retrying => Ok(true),
            Settlement::Settled => Ok(false),
        }
    }

    /// The reaction to `delivery` succeeded.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn ack(&self, delivery: &Delivery, note: Option<&str>) -> Result<(), InboxError> {
        self.settle(delivery, Disposition::Acknowledge(note))
            .map(drop)
    }

    /// While holding `delivery`, acknowledge every not-yet-handled event of
    /// `kinds` up to `through` as covered by its reaction (a consumer whose
    /// reaction considers everything recorded so far need not react once
    /// per event). Leased deliveries are left alone. Returns how many were
    /// covered.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held (nothing
    /// is covered); otherwise SQLite failure.
    pub fn ack_through<K: AsRef<str>>(
        &mut self,
        delivery: &Delivery,
        kinds: &[K],
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
            params![self.consumer, delivery.event.id.0, delivery.token.0],
            |r| r.get(0),
        )?;
        if !holding {
            return Err(InboxError::LeaseLost {
                consumer: self.consumer.clone(),
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
                params![self.consumer, through.0, note],
            )?
        };
        tx.commit()?;
        Ok(changed)
    }

    /// Give `delivery` back undelivered (the worker is stopping); the
    /// attempt does not count.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn release(&self, delivery: &Delivery) -> Result<(), InboxError> {
        self.settle(delivery, Disposition::Release).map(drop)
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
        self.settle(
            delivery,
            Disposition::Retry {
                note: error,
                max_attempts,
            },
        )
    }

    /// The reaction to `delivery` failed and must not be retried, whatever
    /// attempts are left.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn give_up(&self, delivery: &Delivery, error: &str) -> Result<(), InboxError> {
        self.settle(delivery, Disposition::Fail(error)).map(drop)
    }

    /// Deliveries of `kinds` whose holder died on their last attempt: the
    /// lease expired after `max_attempts` attempts, so they are never
    /// leased again. Each awaits [`give_up_abandoned`](Self::give_up_abandoned)
    /// once whatever its holder left behind has been cleaned up.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn abandoned<K: AsRef<str>>(
        &self,
        kinds: &[K],
        max_attempts: i64,
    ) -> Result<Vec<Delivery>, rusqlite::Error> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = self.conn.prepare(&format!(
            // Revoked leases and leases from before tokens existed read as
            // the empty token, which `give_up_abandoned` matches.
            "SELECT e.id, e.kind, e.subject, e.created_at, d.attempts, \
             COALESCE(d.lease_token, '') \
             FROM deliveries d JOIN events e ON e.id = d.event_id \
             WHERE d.agent = ?1 AND e.kind IN ({kinds}) AND d.status = 'leased' \
             AND d.leased_until < {EPOCH} AND d.attempts >= ?2 ORDER BY e.id",
            kinds = kinds_sql(kinds)
        ))?;
        let rows = statement.query_map(params![self.consumer, max_attempts], |r| {
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
            let (id, kind, subject, created_at, attempts, token) = row?;
            Ok(Delivery {
                event: Event {
                    id: EventId(id),
                    kind,
                    subject,
                    created_at,
                },
                attempts,
                token: LeaseToken(token),
            })
        })
        .collect()
    }

    /// Record an [abandoned](Self::abandoned) delivery as failed, noting
    /// that its holder died. `false` when it is no longer abandoned under
    /// that lease (another worker gave it up first).
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
            params![self.consumer, delivery.event.id.0, delivery.token.0],
        )?;
        Ok(changed == 1)
    }

    /// Whether `delivery` is still this holder's, without extending its
    /// lease: what a command asks when it is told to stop.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn holds(&self, delivery: &Delivery) -> Result<bool, rusqlite::Error> {
        self.lease_identity(delivery).is_current(&self.conn)
    }

    /// Extend the lease of `delivery` while its token is current, even
    /// after expiry.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn renew(&self, delivery: &Delivery) -> Result<(), InboxError> {
        if self
            .lease_identity(delivery)
            .renew(&self.conn, LEASE_SECONDS)?
        {
            Ok(())
        } else {
            Err(self.lost(delivery))
        }
    }

    /// Events of `kinds` not yet handled by this consumer (pending, never
    /// delivered, or leased).
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn backlog<K: AsRef<str>>(&self, kinds: &[K]) -> Result<usize, rusqlite::Error> {
        self.count_backlog(kinds, None)
    }

    /// Backlog for this consumer's registered subscription and routing.
    /// Unassigned events count until a worker records their recipient.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn registered_backlog(
        &self,
        registration: &Registration,
    ) -> Result<usize, rusqlite::Error> {
        let Some(consumer) = registration.consumer.get(&self.consumer) else {
            return Ok(0);
        };
        let route = registration.route_for(&self.consumer);
        if route
            .as_ref()
            .is_some_and(|route| !route.members().contains(self.consumer.as_str()))
        {
            return Ok(0);
        }
        self.count_backlog(&consumer.on, route.as_ref())
    }

    fn count_backlog<K: AsRef<str>>(
        &self,
        kinds: &[K],
        route: Option<&Route<'_>>,
    ) -> Result<usize, rusqlite::Error> {
        if kinds.is_empty() {
            return Ok(0);
        }
        let group = route.map(Route::group);
        let members = route.map(Route::eligible_owners);
        let count: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM events e \
                 LEFT JOIN deliveries d ON d.agent = :agent AND d.event_id = e.id \
                 LEFT JOIN assignments a ON a.role = :group AND a.event_id = e.id \
                 WHERE e.kind IN ({kinds}) \
                 AND {route_filter} \
                 AND (d.event_id IS NULL OR d.status IN ('pending', 'leased') OR {routed_away})",
                kinds = kinds_sql(kinds),
                route_filter = route_filter(),
                routed_away = routed_away()
            ),
            named_params! {":agent": self.consumer, ":group": group, ":members": members},
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Record `proposed` as the recipient of `event` in `group` unless one
    /// is recorded already, and return the recorded one: consumers that
    /// would choose differently still agree on the first choice.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn assign(
        &self,
        group: &str,
        event: EventId,
        proposed: &str,
    ) -> Result<String, rusqlite::Error> {
        self.conn.query_row(
            "INSERT INTO assignments (role, event_id, agent) VALUES (?1, ?2, ?3) \
             ON CONFLICT (role, event_id) DO UPDATE SET agent = assignments.agent \
             RETURNING agent",
            params![group, event.0, proposed],
            |r| r.get(0),
        )
    }

    /// Move `event` to `to` if it is still assigned to `from`.
    /// Returns its current recipient.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn reassign(
        &self,
        group: &str,
        event: EventId,
        from: &str,
        to: &str,
    ) -> Result<String, rusqlite::Error> {
        self.conn.execute(
            "UPDATE assignments SET agent = ?4 WHERE role = ?1 AND event_id = ?2 AND agent = ?3",
            params![group, event.0, from, to],
        )?;
        self.conn.query_row(
            "SELECT agent FROM assignments WHERE role = ?1 AND event_id = ?2",
            params![group, event.0],
            |r| r.get(0),
        )
    }

    /// The recipient of `event` in `group`, if assigned.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn assignment(
        &self,
        group: &str,
        event: EventId,
    ) -> Result<Option<String>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT agent FROM assignments WHERE role = ?1 AND event_id = ?2",
                params![group, event.0],
                |r| r.get(0),
            )
            .optional()
    }

    /// The delivery status of `event` for this consumer, if it was ever
    /// delivered.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn status_of(&self, event: EventId) -> Result<Option<String>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT status FROM deliveries WHERE agent = ?1 AND event_id = ?2",
                params![self.consumer, event.0],
                |r| r.get(0),
            )
            .optional()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;

    use super::*;
    use crate::event::EventLog;
    use crate::registration::{ConsumerRegistration, RouterDeclaration, RoutingPolicy};

    const WORK: &[&str] = &["work.found"];

    struct Fixture {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        db: Connection,
    }

    impl Fixture {
        fn inbox(&self, consumer: &str) -> Inbox {
            Inbox::open(&self.path, consumer).expect("inbox")
        }

        fn publish(&self, subject: Option<i64>) -> EventId {
            EventLog::publish(&self.db, WORK[0], subject).expect("event")
        }
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("explorer.db");
        let db = store::open(&path).expect("database");
        store::initialize(&db).expect("tables");
        Fixture {
            _dir: dir,
            path,
            db,
        }
    }

    fn lost<T: fmt::Debug>(result: Result<T, InboxError>) -> bool {
        matches!(result, Err(InboxError::LeaseLost { .. }))
    }

    #[test]
    fn the_highest_priority_goes_first_and_the_oldest_among_equals() {
        let f = fixture();
        let first = f.publish(None);
        let urgent = f.publish(None);
        let second = f.publish(None);
        let most = f.publish(None);
        EventLog::prioritize(&f.db, urgent, 5).expect("priority");
        EventLog::prioritize(&f.db, most, 9).expect("priority");
        let mut inbox = f.inbox("worker");
        assert_eq!(inbox.backlog(WORK).expect("backlog"), 4);
        let mut order = Vec::new();
        while let Some(delivery) = inbox.lease(WORK, 1, false, 3).expect("lease") {
            // The priority that counts is the one it has when it is leased.
            if delivery.event.id == most {
                EventLog::prioritize(&f.db, second, 1).expect("priority");
            }
            inbox.ack(&delivery, None).expect("ack");
            order.push(delivery.event.id);
        }
        assert_eq!(order, [most, urgent, second, first]);
        assert_eq!(inbox.backlog(WORK).expect("backlog"), 0);
        // Another consumer has its own inbox over the same log.
        assert_eq!(f.inbox("other").backlog(WORK).expect("backlog"), 4);
        // A kind nobody published, and a kind that needs quoting.
        assert!(
            inbox
                .lease(&["it's.new"], 1, false, 3)
                .expect("lease")
                .is_none()
        );
    }

    #[test]
    fn events_about_one_subject_go_to_one_worker_at_a_time() {
        let f = fixture();
        let about_seven = f.publish(Some(7));
        let seven_again = f.publish(Some(7));
        let about_eight = f.publish(Some(8));
        let mut one = f.inbox("worker");
        let mut two = f.inbox("worker");
        let held = one.lease(WORK, 1, false, 3).expect("lease").expect("e1");
        assert_eq!(held.event.id, about_seven);
        assert_eq!(held.event.to_string(), "e1 work.found #7");
        // The other event about #7 waits; the one about #8 does not.
        let other = two.lease(WORK, 2, false, 3).expect("lease").expect("e3");
        assert_eq!(other.event.id, about_eight);
        assert!(two.lease(WORK, 2, false, 3).expect("lease").is_none());
        one.ack(&held, Some("done")).expect("ack");
        assert_eq!(
            two.lease(WORK, 2, false, 3)
                .expect("lease")
                .expect("e2")
                .event
                .id,
            seven_again
        );

        // An exclusive consumer reacts to one event at a time, whatever
        // they are about.
        let mut planner = f.inbox("planner");
        let planning = planner.lease(WORK, 3, true, 3).expect("lease").expect("e1");
        assert!(matches!(
            f.inbox("planner")
                .lease_with::<_, (), rusqlite::Error>(WORK, 4, true, 3, None, || Ok(Some(()))),
            Ok(Lease::AtCapacity)
        ));
        planner.ack(&planning, None).expect("ack");
        assert!(planner.lease(WORK, 3, true, 3).expect("lease").is_some());
    }

    #[test]
    fn a_failure_is_retried_within_its_attempts_and_a_release_does_not_count() {
        let f = fixture();
        let event = f.publish(None);
        let mut inbox = f.inbox("worker");
        let first = inbox.lease(WORK, 1, false, 2).expect("lease").expect("e1");
        assert_eq!(first.attempts, 1);
        inbox.renew(&first).expect("renew");
        inbox.release(&first).expect("release");
        // Settled once: the token is spent.
        assert!(lost(inbox.release(&first)));
        assert!(lost(inbox.renew(&first)));

        let again = inbox.lease(WORK, 1, false, 2).expect("lease").expect("e1");
        assert_eq!(again.attempts, 1, "a release is not an attempt");
        assert!(inbox.fail(&again, "it broke", 2).expect("fail"), "retried");
        assert_eq!(
            inbox.status_of(event).expect("status").as_deref(),
            Some("pending")
        );
        let last = inbox.lease(WORK, 2, false, 2).expect("lease").expect("e1");
        assert_eq!(last.attempts, 2);
        assert!(!inbox.fail(&last, "it broke again", 2).expect("fail"));
        assert_eq!(
            inbox.status_of(event).expect("status").as_deref(),
            Some("failed")
        );
        assert!(inbox.lease(WORK, 2, false, 2).expect("lease").is_none());

        // Given up at once, whatever attempts are left.
        let hopeless = f.publish(None);
        let held = inbox.lease(WORK, 2, false, 2).expect("lease").expect("e2");
        inbox.give_up(&held, "cannot work").expect("give up");
        assert_eq!(
            inbox.status_of(hopeless).expect("status").as_deref(),
            Some("failed")
        );
        assert!(inbox.abandoned(WORK, 2).expect("abandoned").is_empty());
    }

    #[test]
    fn a_holder_that_died_on_its_last_attempt_leaves_the_delivery_abandoned() {
        let f = fixture();
        let event = f.publish(Some(3));
        let mut inbox = f.inbox("worker");
        let held = inbox.lease(WORK, 9, false, 1).expect("lease").expect("e1");
        f.db.execute(
            &format!("UPDATE deliveries SET leased_until = {EPOCH} - 1"),
            [],
        )
        .expect("expire");
        // Its only attempt is used up: never leased again, and not settled.
        assert!(inbox.lease(WORK, 1, false, 1).expect("lease").is_none());
        let abandoned = inbox.abandoned(WORK, 1).expect("abandoned");
        assert_eq!(abandoned, std::slice::from_ref(&held));
        assert!(inbox.give_up_abandoned(&held).expect("give up"));
        assert!(!inbox.give_up_abandoned(&held).expect("once"));
        let note: String =
            f.db.query_row(
                "SELECT note FROM deliveries WHERE event_id = ?1",
                [event.0],
                |row| row.get(0),
            )
            .expect("note");
        assert!(note.contains("process 9, died during attempt 1"), "{note}");
    }

    #[test]
    fn one_reaction_can_cover_everything_recorded_before_it() {
        let f = fixture();
        let first = f.publish(None);
        let second = f.publish(None);
        let third = f.publish(None);
        let mut inbox = f.inbox("planner");
        let held = inbox.lease(WORK, 1, true, 3).expect("lease").expect("e1");
        assert_eq!(held.event.id, first);
        // The held delivery is the reaction's own to acknowledge.
        assert_eq!(
            inbox
                .ack_through(&held, WORK, second, "covered by the reaction to e1")
                .expect("cover"),
            1
        );
        inbox.ack(&held, Some("planned")).expect("ack");
        assert!(lost(inbox.ack_through(&held, WORK, third, "late")));
        assert_eq!(
            inbox.status_of(second).expect("status").as_deref(),
            Some("done")
        );
        let next = inbox.lease(WORK, 1, true, 3).expect("lease").expect("e3");
        assert_eq!(next.event.id, third);
    }

    #[test]
    fn a_routed_group_gives_each_event_one_recipient() {
        let f = fixture();
        let mut registration = Registration::default();
        for name in ["explorer", "fringe"] {
            registration.consumer.insert(
                name.to_owned(),
                ConsumerRegistration {
                    on: vec![WORK[0].to_owned()],
                    group: Some("exploration".to_owned()),
                    ..ConsumerRegistration::default()
                },
            );
        }
        registration.router.insert(
            "explorers".to_owned(),
            RouterDeclaration {
                targets: vec!["explorer".to_owned(), "fringe".to_owned()],
                policy: RoutingPolicy::Weighted,
                concurrency: NonZeroUsize::new(2).expect("limit"),
                weights: BTreeMap::new(),
            },
        );
        registration
            .routing
            .insert("exploration".to_owned(), "explorers".to_owned());
        registration.validate().expect("valid");
        let events: Vec<EventId> = (0..40).map(|_| f.publish(None)).collect();

        let mut received: BTreeMap<EventId, Vec<&str>> = BTreeMap::new();
        for name in ["explorer", "fringe"] {
            let route = registration.route_for(name).expect("route");
            let mut inbox = f.inbox(name);
            // Unassigned events count for every member until one of them
            // records the recipient.
            assert!(inbox.registered_backlog(&registration).expect("backlog") > 0);
            loop {
                match inbox
                    .lease_with::<_, (), rusqlite::Error>(WORK, 1, false, 3, Some(&route), || {
                        Ok(Some(()))
                    })
                    .expect("lease")
                {
                    Lease::Granted(delivery, ()) => {
                        assert_eq!(route.select(WORK[0], delivery.event.id), name);
                        assert_eq!(
                            inbox
                                .assignment("exploration", delivery.event.id)
                                .expect("assignment")
                                .as_deref(),
                            Some(name)
                        );
                        inbox.ack(&delivery, None).expect("ack");
                        received.entry(delivery.event.id).or_default().push(name);
                    }
                    Lease::Idle => break,
                    Lease::AtCapacity => panic!("there was room"),
                }
            }
            assert_eq!(inbox.registered_backlog(&registration).expect("backlog"), 0);
        }
        assert_eq!(received.keys().copied().collect::<Vec<_>>(), events);
        assert!(received.values().all(|consumers| consumers.len() == 1));
        for name in ["explorer", "fringe"] {
            let share = received.values().filter(|to| to[0] == name).count();
            assert!((8..=32).contains(&share), "{name}: {share} of 40");
        }
        // No room: the event stays unleased, its recipient recorded.
        let waiting = f.publish(None);
        let recipient = registration
            .route_for("explorer")
            .expect("route")
            .select(WORK[0], waiting)
            .to_owned();
        let route = registration.route_for(&recipient).expect("route");
        let mut inbox = f.inbox(&recipient);
        assert!(matches!(
            inbox
                .lease_with::<_, (), rusqlite::Error>(WORK, 1, false, 3, Some(&route), || Ok(None)),
            Ok(Lease::AtCapacity)
        ));
        assert_eq!(inbox.status_of(waiting).expect("status"), None);
        assert_eq!(inbox.registered_backlog(&registration).expect("backlog"), 1);
    }
}
