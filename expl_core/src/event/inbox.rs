//! [`Inbox`] — one consumer's view of the event log, in the graph's
//! vocabulary. Delivery itself is the runtime's
//! ([`expl_runtime::delivery`]); this is the same inbox with event kinds
//! and nodes typed.

use std::path::Path;

use expl_runtime::delivery as runtime;

use crate::declaration::{Declaration, Route};
use crate::event::{Event, EventId, EventKind};
use crate::graph::NodeId;
use crate::process::ProcessId;
use crate::store::{self, StoreError};

pub use expl_runtime::delivery::{InboxError, LEASE_SECONDS, LeaseToken};

/// An event leased to one process of a consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The event.
    pub event: Event,
    /// How many times it has been leased, this time included.
    pub attempts: i64,
    /// This lease's token, required to renew or settle it.
    pub token: LeaseToken,
}

impl Delivery {
    /// The delivery as the runtime holds it.
    fn untyped(&self) -> runtime::Delivery {
        runtime::Delivery {
            event: runtime::Event {
                id: self.event.id,
                kind: self.event.kind.as_str().to_owned(),
                subject: self.event.node.map(|node| node.0),
                created_at: self.event.created_at.clone(),
            },
            attempts: self.attempts,
            token: self.token.clone(),
        }
    }

    /// The runtime's delivery, when its event is one the graph records.
    fn typed(delivery: runtime::Delivery) -> Result<Self, rusqlite::Error> {
        let kind = delivery.event.kind.parse().map_err(|reason: String| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(reason)),
            )
        })?;
        Ok(Self {
            event: Event {
                id: delivery.event.id,
                kind,
                node: delivery.event.subject.map(NodeId),
                created_at: delivery.event.created_at,
            },
            attempts: delivery.attempts,
            token: delivery.token,
        })
    }
}

pub(crate) enum Lease<T> {
    Idle,
    AtCapacity,
    Granted(Delivery, T),
}

/// The deliveries of one consumer. See the [module docs](crate::event).
#[derive(Debug)]
pub struct Inbox {
    inbox: runtime::Inbox,
}

fn names(kinds: &[EventKind]) -> Vec<&'static str> {
    kinds.iter().map(|kind| kind.as_str()).collect()
}

impl Inbox {
    /// The inbox of `consumer` in the database at `path`.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the database cannot be opened.
    pub fn open(path: &Path, consumer: &str) -> Result<Self, StoreError> {
        Ok(Self {
            inbox: runtime::Inbox::over(store::open(path)?, consumer),
        })
    }

    /// The consumer this inbox belongs to.
    #[must_use]
    pub fn consumer(&self) -> &str {
        self.inbox.consumer()
    }

    /// Trace context of the producer of this durable event, if it had one.
    /// A telemetry lookup must never affect delivery of the event itself.
    pub(crate) fn cause(&self, id: EventId) -> Option<opentelemetry::trace::SpanContext> {
        let (parent, state) = self.inbox.trace_context(id)?;
        crate::telemetry::extract(parent, state)
    }

    /// Lease the next deliverable event of `kinds` to `process`: the
    /// oldest, except that `node.ready` events go first, highest current
    /// priority of their step first, so the most promising step is tried
    /// first however long ago it became ready.
    ///
    /// Deliverable: never delivered to this consumer, released, or leased by a
    /// process whose lease expired before its `max_attempts`-th attempt.
    /// Every lease counts as an attempt, so a delivery whose holders keep
    /// dying is [abandoned](Self::abandoned) rather than leased forever.
    /// For this consumer, an unexpired lease blocks other events about its node.
    /// With `exclusive`, an unexpired lease blocks every event (single-flight
    /// handlers). Leasing an event replaces its previous token and revokes any
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
        self.inbox
            .lease(&names(kinds), process.0, exclusive, max_attempts)?
            .map(Delivery::typed)
            .transpose()
    }

    /// Find or record an event's recipient before taking a delivery. Try
    /// to acquire capacity only for work that belongs to this consumer, and
    /// leave the event unleased when no slot is available.
    pub(crate) fn lease_with<T, E: From<rusqlite::Error>>(
        &mut self,
        kinds: &[EventKind],
        process: ProcessId,
        exclusive: bool,
        max_attempts: i64,
        route: Option<&Route<'_>>,
        try_capacity: impl FnOnce() -> Result<Option<T>, E>,
    ) -> Result<Lease<T>, E> {
        Ok(
            match self.inbox.lease_with(
                &names(kinds),
                process.0,
                exclusive,
                max_attempts,
                route,
                try_capacity,
            )? {
                runtime::Lease::Idle => Lease::Idle,
                runtime::Lease::AtCapacity => Lease::AtCapacity,
                runtime::Lease::Granted(delivery, capacity) => {
                    Lease::Granted(Delivery::typed(delivery)?, capacity)
                }
            },
        )
    }

    /// The reaction to `delivery` succeeded.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn ack(&self, delivery: &Delivery, note: Option<&str>) -> Result<(), InboxError> {
        self.inbox.ack(&delivery.untyped(), note)
    }

    /// While holding `delivery`, acknowledge every not-yet-handled event of
    /// `kinds` up to `through` as covered by its reaction (handlers that
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
        self.inbox
            .ack_through(&delivery.untyped(), &names(kinds), through, note)
    }

    /// Give `delivery` back undelivered (the process is stopping); the
    /// attempt does not count.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn release(&self, delivery: &Delivery) -> Result<(), InboxError> {
        self.inbox.release(&delivery.untyped())
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
        self.inbox.fail(&delivery.untyped(), error, max_attempts)
    }

    /// The reaction to `delivery` failed and must not be retried, whatever
    /// attempts are left.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn give_up(&self, delivery: &Delivery, error: &str) -> Result<(), InboxError> {
        self.inbox.give_up(&delivery.untyped(), error)
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
        self.inbox
            .abandoned(&names(kinds), max_attempts)?
            .into_iter()
            .map(Delivery::typed)
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
        self.inbox.give_up_abandoned(&delivery.untyped())
    }

    /// Extend the lease of `delivery` while its token is current, even
    /// after expiry.
    ///
    /// # Errors
    ///
    /// [`InboxError::LeaseLost`] when this lease is no longer held;
    /// otherwise SQLite failure.
    pub fn renew(&self, delivery: &Delivery) -> Result<(), InboxError> {
        self.inbox.renew(&delivery.untyped())
    }

    /// Events of `kinds` not yet handled by this consumer (pending, never
    /// delivered, or leased).
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn backlog(&self, kinds: &[EventKind]) -> Result<usize, rusqlite::Error> {
        self.inbox.backlog(&names(kinds))
    }

    /// Backlog for this consumer's configured subscription and routing.
    /// Unassigned events count until a process records their recipient.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn declared_backlog(&self, declaration: &Declaration) -> Result<usize, rusqlite::Error> {
        self.inbox.registered_backlog(declaration.registration())
    }

    /// Record or read the first assignment for a legacy weighted inbox.
    /// Peers agree on the recipient even while running different weights.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub(crate) fn assign(
        &self,
        group: &str,
        event: EventId,
        proposed: &str,
    ) -> Result<String, rusqlite::Error> {
        self.inbox.assign(group, event, proposed)
    }

    /// Move `event` to `to` if it is still assigned to `from`.
    /// Returns its current recipient.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub(crate) fn reassign(
        &self,
        group: &str,
        event: EventId,
        from: &str,
        to: &str,
    ) -> Result<String, rusqlite::Error> {
        self.inbox.reassign(group, event, from, to)
    }

    /// The recipient of `event` in `group`, if assigned.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub(crate) fn assignment(
        &self,
        group: &str,
        event: EventId,
    ) -> Result<Option<String>, rusqlite::Error> {
        self.inbox.assignment(group, event)
    }

    /// The delivery status of `event` for this consumer, if it was ever
    /// delivered.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn status_of(&self, event: EventId) -> Result<Option<String>, rusqlite::Error> {
        self.inbox.status_of(event)
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;
    use crate::graph::Graph;
    use crate::store::EPOCH;
    use opentelemetry::Context;
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };

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

    #[test]
    fn event_producer_trace_context_survives_a_new_connection_and_redelivery() {
        let mut f = fixture();
        let producer = SpanContext::new(
            TraceId::from(123_u128),
            SpanId::from(456_u64),
            TraceFlags::SAMPLED,
            true,
            TraceState::from_key_value([("vendor", "state")]).expect("trace state"),
        );
        {
            let _scope = Context::new()
                .with_remote_span_context(producer.clone())
                .attach();
            f.graph
                .add_goal("traced", "traced", false, &[])
                .expect("goal");
        }

        let mut inbox = Inbox::open(&f.db, "exploration").expect("inbox");
        let kinds = &[EventKind::GoalAdded];
        let earlier = inbox
            .lease(kinds, ProcessId(1), false, 3)
            .expect("lease")
            .expect("event");
        assert!(inbox.cause(earlier.event.id).is_none());
        inbox.ack(&earlier, None).expect("ack");
        for attempt in 1..=2 {
            let delivery = inbox
                .lease(kinds, ProcessId(attempt), false, 3)
                .expect("lease")
                .expect("event");
            assert_eq!(delivery.attempts, attempt);
            assert_eq!(inbox.cause(delivery.event.id), Some(producer.clone()));
            assert!(inbox.fail(&delivery, "retry", 3).expect("retry"));
        }
    }

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
    fn a_route_reassigns_unfinished_work_when_its_owner_unsubscribes() {
        let f = fixture();
        let group = "planning";
        let mut former = Inbox::open(&f.db, "former").expect("inbox");
        let mut planner = Inbox::open(&f.db, "planner").expect("inbox");
        let lease = |inbox: &mut Inbox| {
            inbox
                .lease(PROPOSED, ProcessId(1), false, 3)
                .expect("lease")
                .expect("event")
        };
        let abandoned = lease(&mut former);
        former
            .assign(group, abandoned.event.id, "former")
            .expect("assignment");
        let ignored = lease(&mut planner);
        assert_eq!(ignored.event.id, abandoned.event.id);
        planner
            .ack(&ignored, Some("ignored: routed to `former`"))
            .expect("ack");
        let complete = lease(&mut former);
        former
            .assign(group, complete.event.id, "former")
            .expect("assignment");
        former.ack(&complete, Some("planned")).expect("ack");

        let declaration = Declaration::parse(&format!(
            "{}{}",
            Declaration::TEMPLATE.replace(
                "[consumer.planner]",
                "[consumer.planner]\ngroup = \"planning\""
            ),
            r#"
[consumer.former]
on = ["node.evaluated"]
handler = "planning"
group = "planning"
driver = "generic"
command = ["planner"]
replicas = 1

[routing]
planning = "planners"

[router.planners]
targets = ["former", "planner"]
policy = "first"
concurrency = 1
"#
        ))
        .expect("declaration");
        let route = declaration.route_for("planner").expect("route");
        let take = |inbox: &mut Inbox| {
            inbox
                .lease_with::<_, rusqlite::Error>(
                    PROPOSED,
                    ProcessId(2),
                    false,
                    3,
                    Some(&route),
                    || Ok(Some(())),
                )
                .expect("lease")
        };
        assert!(matches!(take(&mut planner), Lease::Idle));
        expire(&f.db, "former", abandoned.event.id);
        assert_eq!(planner.declared_backlog(&declaration).expect("backlog"), 1);
        let Lease::Granted(reassigned, ()) = take(&mut planner) else {
            panic!("unfinished event was not reassigned");
        };
        assert_eq!(reassigned.event.id, abandoned.event.id);
        assert_eq!(reassigned.attempts, 1);
        assert_lost(former.renew(&abandoned), abandoned.event.id);
        planner.ack(&reassigned, Some("planned")).expect("ack");
        assert!(matches!(take(&mut planner), Lease::Idle));
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
        f.graph.plan_step(b, 5).expect("plan b");
        let mut inbox = Inbox::open(&f.db, "verifier").expect("inbox");
        let kinds = &[EventKind::NodeReady, EventKind::NodeProposed];
        let mut next = || {
            let delivery = inbox.lease(kinds, ProcessId(1), false, 3).expect("lease")?;
            inbox.ack(&delivery, None).expect("ack");
            Some((delivery.event.kind, delivery.event.node))
        };
        // The planner then ranks a below b: its current priority counts, not
        // the one it had when it became ready, nor which became ready first.
        f.graph.plan_step(a, 2).expect("lower a");
        let order: Vec<_> = std::iter::from_fn(&mut next).collect();
        assert_eq!(
            order,
            vec![
                (EventKind::NodeReady, Some(b)),
                (EventKind::NodeReady, Some(a)),
                // Other kinds carry no priority: after every ready step
                // that has one, oldest first.
                (EventKind::NodeProposed, Some(a)),
                (EventKind::NodeProposed, Some(b)),
            ]
        );

        // A step ranked at nothing still goes after one ranked above it.
        let mut step = |title: &str, priorities: &[i64]| {
            let id = f
                .graph
                .propose_step(title, title, &[NodeId(1)], &[NodeId(2)])
                .expect("step");
            for priority in priorities {
                f.graph.plan_step(id, *priority).expect("plan");
            }
            id
        };
        let unranked = step("unranked", &[9, 0]);
        let ranked = step("ranked", &[7]);
        let mut inbox = Inbox::open(&f.db, "verifier").expect("inbox");
        let ready = &[EventKind::NodeReady];
        let order: Vec<_> = std::iter::from_fn(|| {
            let delivery = inbox.lease(ready, ProcessId(1), false, 3).expect("lease")?;
            inbox.ack(&delivery, None).expect("ack");
            delivery.event.node
        })
        .collect();
        assert_eq!(order, [ranked, unranked]);
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
                            .assign("exploration", event, proposed)
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
        gone.assign("exploration", EventId(2), "removed")
            .expect("assign");
        assert_eq!(
            gone.reassign("exploration", EventId(2), "removed", "explorer")
                .expect("reassign"),
            "explorer"
        );
        assert_eq!(
            gone.reassign("exploration", EventId(2), "removed", "contrarian")
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
                .assign("exploration", event, other)
                .expect("assign"),
            first
        );
        assert_eq!(
            reopened
                .assignment("exploration", event)
                .expect("assignment"),
            Some(first)
        );
        // Each role assigns its events on its own.
        assert_eq!(
            reopened.assignment("planning", event).expect("assignment"),
            None
        );
        assert_eq!(
            reopened
                .assign("planning", event, "planner")
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
