//! [`Process`] — one consumer worker: lease an event, react, acknowledge.

use std::thread;
use std::time::{Duration, Instant};

use expl_runtime::capacity::Capacity;
use expl_runtime::worker::{Active, with_heartbeat};
use opentelemetry::KeyValue;
use thiserror::Error;

use crate::event::{Consumer, Delivery, EventKind, Inbox, InboxError, Lease};
use crate::graph::{Graph, GraphError, NodeId};
use crate::handler::{Abandonment, Context, Handler, HandlerError, Reaction, Report};
use crate::process::telemetry::{Phase, WorkerTelemetry};
use crate::process::{HEARTBEAT_SECONDS, Interrupt, ProcessId, ProcessRecord, Processes};
use crate::store::StoreError;
use crate::telemetry;
use crate::trunk::{Advance, Trunk};

/// How long an idle process waits before looking at its inbox again.
const POLL: Duration = Duration::from_millis(500);

/// How often a process looks whether the trunk moved.
const FOLLOW: Duration = Duration::from_secs(2);

/// How often a failing event is delivered before it is given up.
pub const MAX_ATTEMPTS: i64 = 3;

/// The process could not keep running.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// The database could not be opened.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The database failed.
    #[error("process database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A project-local reaction slot could not be opened or locked.
    #[error("cannot use reaction slots: {0}")]
    Capacity(#[from] std::io::Error),
    /// The graph failed.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// The record was no longer there to adopt: it already started,
    /// exited, or missed its startup deadline (and its replica slot may
    /// have gone to a replacement).
    #[error(
        "process {0} cannot be adopted: it is no longer a created record within its startup \
         deadline"
    )]
    NotAdoptable(ProcessId),
}

/// How long a process lives.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessOptions {
    /// Exit after reacting to one event.
    pub once: bool,
    /// Exit as soon as the inbox has nothing deliverable.
    pub exit_when_idle: bool,
}

/// What a process is doing, for observers (the CLI writes these to the
/// process log).
#[derive(Debug)]
pub enum ProcessActivity<'a> {
    /// The process adopted its record.
    Started(&'a ProcessRecord),
    /// The trunk moved and the origin followed it.
    Followed(&'a Advance),
    /// The trunk cannot be followed (reported once per distinct reason).
    Unfollowed(&'a str),
    /// An event was leased.
    Reacting(&'a Delivery),
    /// The reaction succeeded and the event was acknowledged.
    Reacted {
        /// The delivery.
        delivery: &'a Delivery,
        /// What the handler did.
        report: &'a Report,
    },
    /// The process lost its lease on the event (it expired and another
    /// process may have taken the event over): the reaction was interrupted
    /// or its outcome discarded, and the event was left to the new holder.
    LeaseLost(&'a Delivery),
    /// The reaction failed.
    Failed {
        /// The delivery.
        delivery: &'a Delivery,
        /// What went wrong.
        error: &'a HandlerError,
        /// Whether the event will be delivered again.
        retry: bool,
    },
    /// A delivery whose holder died on its last attempt was given up.
    Abandoned {
        /// The delivery, as its last holder leased it.
        delivery: &'a Delivery,
        /// The step its verification left `running`, now failed.
        failed: Option<NodeId>,
    },
    /// The process is exiting.
    Exiting(&'a str),
}

/// Receives [`ProcessActivity`] as it happens.
pub trait ProcessObserver: Sync {
    /// Observe one activity.
    fn observe(&self, activity: &ProcessActivity<'_>);
}

/// The lease a process is reacting under, shared with its heartbeat.
type Held = Active<Delivery>;

/// `Ok(None)` when the lease was lost.
fn holding<T>(result: Result<T, InboxError>) -> Result<Option<T>, ProcessError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(InboxError::LeaseLost { .. }) => Ok(None),
        Err(InboxError::Sqlite(error)) => Err(error.into()),
    }
}

/// See the [module docs](crate::process).
#[derive(Debug)]
pub struct Process<'a> {
    context: &'a Context,
    consumer: &'a Consumer,
    id: ProcessId,
    interrupt: Interrupt,
}

impl<'a> Process<'a> {
    /// The process registered as `id`, running `consumer`.
    #[must_use]
    pub fn new(
        context: &'a Context,
        consumer: &'a Consumer,
        id: ProcessId,
        interrupt: Interrupt,
    ) -> Self {
        Self {
            context,
            consumer,
            id,
            interrupt,
        }
    }

    /// Adopt the record, react to events until stopped (or as `options`
    /// say), and record the exit. Returns the exit reason.
    ///
    /// # Errors
    ///
    /// [`ProcessError`] when the database fails; the record is marked
    /// exited with the error. [`ProcessError::NotAdoptable`] when the
    /// record cannot be adopted; it is left as it is.
    pub fn run(
        &self,
        options: ProcessOptions,
        observer: &dyn ProcessObserver,
    ) -> Result<String, ProcessError> {
        let project = self.context.project();
        let processes = Processes::open(&project.database_path(), &project.processes_dir())?;
        if !processes.start(self.id, std::process::id())? {
            return Err(ProcessError::NotAdoptable(self.id));
        }
        let record = processes.get(self.id)?;
        let telemetry = WorkerTelemetry::new(observer, self.consumer, &record);
        telemetry.observe(&ProcessActivity::Started(&record));

        let database = project.database_path();
        let mut heartbeat = None;
        let result = with_heartbeat(
            Duration::from_secs(HEARTBEAT_SECONDS),
            move |held| {
                if self
                    .context
                    .authority()
                    .is_some_and(|authority| !authority.is_live())
                {
                    self.interrupt.raise();
                }
                if heartbeat.is_none() {
                    heartbeat = Processes::open(&database, &project.processes_dir())
                        .ok()
                        .zip(Inbox::open(&database, self.consumer.name()).ok());
                }
                if let Some((processes, inbox)) = &heartbeat {
                    self.beat(processes, inbox, held);
                }
            },
            |held| self.serve(&processes, held, options, &telemetry),
        );
        let reason = match &result {
            Ok(reason) => reason.clone(),
            Err(error) => format!("error: {error}"),
        };
        telemetry.observe(&ProcessActivity::Exiting(&reason));
        if self
            .context
            .authority()
            .is_none_or(expl_runtime::cluster::Authority::is_live)
        {
            processes.exit(self.id, &reason)?;
        }
        result
    }

    /// Heartbeat: prove liveness, renew the held lease (interrupting the
    /// reaction when it is lost), and turn a recorded stop request into an
    /// interrupt.
    fn beat(&self, processes: &Processes, inbox: &Inbox, held: &Held) {
        if matches!(processes.heartbeat(self.id), Ok(true)) {
            self.interrupt.raise();
        }
        held.renew(|delivery| !matches!(inbox.renew(delivery), Err(InboxError::LeaseLost { .. })));
    }

    fn serve(
        &self,
        processes: &Processes,
        held: &Held,
        options: ProcessOptions,
        observer: &WorkerTelemetry<'_>,
    ) -> Result<String, ProcessError> {
        let project = self.context.project();
        let database = project.database_path();
        let declaration = self.context.declaration();
        let consumer = self.consumer.declaration();
        let handler = consumer.handler;
        let route = declaration.route_for(self.consumer.name());
        if route
            .as_ref()
            .is_some_and(|route| !route.members().contains(self.consumer.name()))
        {
            return Ok("consumer is not reachable through its delivery group".to_owned());
        }
        let capacity = Capacity::new(
            &project.state_dir(),
            self.consumer.name(),
            consumer.concurrency,
            route.as_ref(),
        )?;
        let shared_capacity = route
            .as_ref()
            .is_some_and(|route| route.groups(self.consumer.name()).next().is_some());
        let mut inbox = Inbox::open(&database, self.consumer.name())?;
        let mut graph = Graph::open(&database)?;
        let trunk = Trunk::new(project, declaration.trunk.as_deref()).with_remote(
            project,
            declaration.workspace.remote.as_deref(),
            &self.interrupt,
        );
        let weighted = route.is_none()
            && declaration.legacy_shares_with(self.consumer.name(), self.consumer.name());
        let mut activity = "";
        let mut followed: Option<Instant> = None;
        let mut unfollowed: Option<String> = None;
        // Starts the first cooldown: a fresh process reacts to its backlog
        // before it wanders.
        let mut last_reaction = Instant::now();
        observer.state(Phase::WaitingForEvents);
        loop {
            if self.interrupt.is_raised() {
                return Ok("stopped".to_owned());
            }
            // Every process follows the trunk, so a merge is noticed while
            // any worker runs; the graph records each move only once.
            if followed.is_none_or(|at| at.elapsed() >= FOLLOW) {
                followed = Some(Instant::now());
                match trunk.follow(&mut graph) {
                    Ok(Some(advance)) => {
                        unfollowed = None;
                        observer.observe(&ProcessActivity::Followed(&advance));
                    }
                    Ok(None) => unfollowed = None,
                    Err(error) => {
                        let reason = error.to_string();
                        if unfollowed.as_deref() != Some(reason.as_str()) {
                            observer.observe(&ProcessActivity::Unfollowed(&reason));
                            unfollowed = Some(reason);
                        }
                    }
                }
            }
            let owner = weighted.then(|| self.consumer.name());
            give_up_abandoned(handler, &consumer.on, owner, &inbox, &mut graph, observer)?;
            let (delivery, slots) = match inbox.lease_with(
                &consumer.on,
                self.id,
                handler.is_exclusive(),
                MAX_ATTEMPTS,
                route.as_ref(),
                || capacity.try_acquire().map_err(ProcessError::from),
            )? {
                Lease::Granted(delivery, slots) => (delivery, slots),
                Lease::AtCapacity => {
                    observer.state(Phase::WaitingForCapacity);
                    if activity != "waiting for capacity" {
                        activity = "waiting for capacity";
                        processes.set_activity(self.id, Some(activity))?;
                    }
                    thread::sleep(POLL);
                    continue;
                }
                Lease::Idle => {
                    observer.state(Phase::WaitingForEvents);
                    if options.exit_when_idle {
                        return Ok("idle".to_owned());
                    }
                    // An idle consumer can also produce a revisit event.
                    if consumer.on.contains(&EventKind::NodeRevisited)
                        && consumer.revisit_cooldown.is_some_and(|seconds| {
                            last_reaction.elapsed() >= Duration::from_secs(seconds)
                        })
                        && revisit_at_random(&mut graph, consumer.speculate == Some(true))?
                    {
                        last_reaction = Instant::now();
                        continue;
                    }
                    if activity != "waiting for events" {
                        activity = "waiting for events";
                        processes.set_activity(self.id, Some(activity))?;
                    }
                    thread::sleep(POLL);
                    continue;
                }
            };
            let trace = observer.delivery(&inbox, &delivery);
            observer.state(Phase::Reacting);
            // Legacy weighted consumers react only to the events
            // assigned to them; the others are theirs to acknowledge. The
            // assignment is recorded before either, so processes that
            // loaded different weights still agree on the one recipient.
            let proposal = weighted
                .then(|| declaration.legacy_route(self.consumer.name(), delivery.event.id))
                .flatten();
            let owner = match proposal {
                Some(proposed) => {
                    let owner = inbox.assign(handler.as_str(), delivery.event.id, proposed)?;
                    // Assigned to an agent no longer declared for the role
                    // (by a process that loaded an older declaration): it
                    // would never be handled, so move it.
                    if owner != proposed
                        && !declaration.legacy_shares_with(self.consumer.name(), &owner)
                    {
                        Some(inbox.reassign(
                            handler.as_str(),
                            delivery.event.id,
                            &owner,
                            proposed,
                        )?)
                    } else {
                        Some(owner)
                    }
                }
                None => None,
            };
            if let Some(owner) = owner.filter(|owner| owner != self.consumer.name()) {
                let report = Report::Ignored {
                    reason: format!("routed to `{owner}`"),
                };
                if holding(inbox.ack(&delivery, Some(&report.summary())))?.is_some() {
                    observer.observe(&ProcessActivity::Reacted {
                        delivery: &delivery,
                        report: &report,
                    });
                } else {
                    observer.observe(&ProcessActivity::LeaseLost(&delivery));
                }
                observer.state(Phase::WaitingForEvents);
                continue;
            }
            processes.set_activity(self.id, Some(&format!("reacting to {}", delivery.event)))?;
            activity = "";
            observer.observe(&ProcessActivity::Reacting(&delivery));

            let covered = graph.last_event()?;
            let interrupt = self.interrupt.for_reaction();
            let reaction =
                Reaction::new(self.context, self.consumer, self.id, &delivery, &interrupt);
            let result = held.react(delivery.clone(), &interrupt, || {
                handler.react(&reaction, &mut graph)
            });
            let lost = || observer.observe(&ProcessActivity::LeaseLost(&delivery));
            match result {
                _ if interrupt.lease_lost() => lost(),
                Err(HandlerError::Graph(GraphError::LeaseLost(_))) => lost(),
                Ok(report) => {
                    let coalesced = match report.covered().or(covered) {
                        Some(through) if handler.coalesces() => {
                            let note = format!("covered by the reaction to {}", delivery.event.id);
                            holding(inbox.ack_through(&delivery, &consumer.on, through, &note))?
                                .is_some()
                        }
                        Some(_) | None => true,
                    };
                    if coalesced
                        && holding(inbox.ack(&delivery, Some(&report.summary())))?.is_some()
                    {
                        observer.observe(&ProcessActivity::Reacted {
                            delivery: &delivery,
                            report: &report,
                        });
                    } else {
                        lost();
                    }
                }
                Err(error) if self.interrupt.is_raised() => {
                    if holding(inbox.release(&delivery))?.is_some() {
                        observer.observe(&ProcessActivity::Failed {
                            delivery: &delivery,
                            error: &error,
                            retry: true,
                        });
                    } else {
                        lost();
                    }
                    return Ok("stopped".to_owned());
                }
                // A busy database is transient (another process held the
                // lock past the busy timeout): give the event back and carry
                // on, rather than lose the process.
                Err(HandlerError::Graph(error)) if error.is_busy() => {
                    let error = HandlerError::Graph(error);
                    if holding(inbox.release(&delivery))?.is_some() {
                        observer.observe(&ProcessActivity::Failed {
                            delivery: &delivery,
                            error: &error,
                            retry: true,
                        });
                    } else {
                        lost();
                    }
                    thread::sleep(POLL * 4);
                }
                // The step was settled elsewhere meanwhile: there is
                // nothing left to do, and retrying would change nothing.
                Err(HandlerError::Graph(error))
                    if delivery.event.node.is_some_and(|id| error.is_settled(id)) =>
                {
                    if !acknowledge_settled(&inbox, &delivery, observer)? {
                        lost();
                    }
                }
                // Any other failure, a graph error included (often a race
                // with another process), is counted and retried; the last
                // attempt fails the step.
                Err(error) => {
                    let why = error.to_string();
                    // The last attempt: clean up before the delivery is
                    // settled, so dying in between leaves it to be retried
                    // or abandoned rather than lost.
                    let abandoned = if delivery.attempts >= MAX_ATTEMPTS {
                        handler.abandon(&delivery, &mut graph, &why)?
                    } else {
                        Abandonment::Nothing
                    };
                    if let Abandonment::Settled(_) = abandoned {
                        if !acknowledge_settled(&inbox, &delivery, observer)? {
                            lost();
                        }
                    } else {
                        match holding(inbox.fail(&delivery, &why, MAX_ATTEMPTS))? {
                            Some(retry) => observer.observe(&ProcessActivity::Failed {
                                delivery: &delivery,
                                error: &error,
                                retry,
                            }),
                            None => lost(),
                        }
                    }
                }
            }
            drop(trace);
            observer.state(Phase::WaitingForEvents);
            last_reaction = Instant::now();
            processes.set_activity(self.id, None)?;
            if options.once {
                return Ok("reacted to one event".to_owned());
            }
            drop(slots);
            if shared_capacity {
                // Let workers polling the same router see the slot before a
                // worker with a deep backlog tries to take it again.
                thread::sleep(POLL);
            }
        }
    }
}

/// Give up every delivery of the subscribed kinds whose holder died on its last attempt,
/// failing first what its reaction left `running`. A delivery whose step was
/// settled elsewhere meanwhile is acknowledged as ignored instead: nothing
/// of it was left hanging. An agent sharing its role by weight
/// (`weighted_as`, its name) fails only what it was assigned: an event
/// assigned to another agent is that agent's to carry out, whatever became
/// of this agent's delivery of it.
fn give_up_abandoned(
    handler: Handler,
    kinds: &[EventKind],
    weighted_as: Option<&str>,
    inbox: &Inbox,
    graph: &mut Graph,
    observer: &dyn ProcessObserver,
) -> Result<(), ProcessError> {
    for delivery in inbox.abandoned(kinds, MAX_ATTEMPTS)? {
        let why = format!(
            "abandoned: the process reacting to {} died on attempt {} of {MAX_ATTEMPTS}",
            delivery.event, delivery.attempts
        );
        let ours = match weighted_as {
            None => true,
            Some(agent) => {
                inbox
                    .assignment(handler.as_str(), delivery.event.id)?
                    .as_deref()
                    == Some(agent)
            }
        };
        let abandoned = if ours {
            handler.abandon(&delivery, graph, &why)?
        } else {
            Abandonment::Nothing
        };
        if let Abandonment::Settled(_) = abandoned
            && acknowledge_settled(inbox, &delivery, observer)?
        {
            continue;
        }
        if inbox.give_up_abandoned(&delivery)? {
            observer.observe(&ProcessActivity::Abandoned {
                delivery: &delivery,
                failed: abandoned.failed(),
            });
        }
    }
    Ok(())
}

/// Acknowledge `delivery` with an [ignored](Report::Ignored) report: its
/// step was settled elsewhere, so no reaction is left to make. `false` when
/// the delivery's lease is no longer held.
fn acknowledge_settled(
    inbox: &Inbox,
    delivery: &Delivery,
    observer: &dyn ProcessObserver,
) -> Result<bool, ProcessError> {
    let step = delivery
        .event
        .node
        .map_or_else(|| "its step".to_owned(), |id| id.to_string());
    let report = Report::Ignored {
        reason: format!("{step} was settled elsewhere"),
    };
    if holding(inbox.ack(delivery, Some(&report.summary())))?.is_none() {
        return Ok(false);
    }
    observer.observe(&ProcessActivity::Reacted {
        delivery,
        report: &report,
    });
    Ok(true)
}

/// Record `node.revisited` for a random starting point (steps not carried
/// out yet among them when exploring `ahead`); `false` when there is none
/// (no active goals).
fn revisit_at_random(graph: &mut Graph, ahead: bool) -> Result<bool, ProcessError> {
    let points = graph.starting_points(ahead)?;
    if points.is_empty() {
        return Ok(false);
    }
    let node = points[random_below(points.len())].id;
    telemetry::operation(
        "expl.exploration.revisit",
        vec![
            KeyValue::new("expl.node.id", node.0),
            KeyValue::new("expl.exploration.speculate", ahead),
        ],
        "graph",
        || graph.revisit(node, ahead),
    )?;
    Ok(true)
}

/// A number in `0..bound` (`bound` > 0), random enough to spread
/// explorations over the starting points.
fn random_below(bound: usize) -> usize {
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is seeded randomly per instance.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    (hasher.finish() % bound as u64) as usize
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::event::{EventId, EventKind};
    use crate::graph::{Answer, NodeStatus};
    use crate::store::{self, EPOCH};

    /// Remembers which deliveries were abandoned.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<(EventId, Option<NodeId>)>>);

    impl ProcessObserver for Recorder {
        fn observe(&self, activity: &ProcessActivity<'_>) {
            if let ProcessActivity::Abandoned { delivery, failed } = activity {
                self.0
                    .lock()
                    .expect("lock")
                    .push((delivery.event.id, *failed));
            }
        }
    }

    #[test]
    fn a_verification_whose_holders_keep_dying_fails_its_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let mut graph = Graph::open(&db).expect("graph");
        let origin = graph
            .create_origin("o", Some("main"), "abc")
            .expect("origin");
        let goal = graph.add_goal("g", "g", false, &[]).expect("goal");
        let step = graph
            .propose_step("s", "s", &[origin], &[goal])
            .expect("step");
        graph.plan_step(step, 1).expect("plan"); // node.ready
        let role = Handler::Verification;
        let mut inbox = Inbox::open(&db, "verifier").expect("inbox");
        let mut ready = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let delivery = inbox
                .lease(role.events(), ProcessId(attempt), false, MAX_ATTEMPTS)
                .expect("lease")
                .expect("node.ready");
            assert_eq!(
                (delivery.event.kind, delivery.event.node, delivery.attempts),
                (EventKind::NodeReady, Some(step), attempt)
            );
            ready = Some(delivery.event.id);
            // Each holder claims the step, then dies without settling.
            assert!(graph.claim(step, &delivery.token).expect("claim"));
            store::open(&db)
                .expect("db")
                .execute(
                    &format!("UPDATE deliveries SET leased_until = {EPOCH} - 1"),
                    [],
                )
                .expect("expire");
            let recorder = Recorder::default();
            give_up_abandoned(role, role.events(), None, &inbox, &mut graph, &recorder)
                .expect("give up");
            let abandoned = recorder.0.into_inner().expect("lock");
            if attempt < MAX_ATTEMPTS {
                assert!(abandoned.is_empty(), "attempts are left");
                assert_eq!(graph.node(step).expect("step").status, NodeStatus::Running);
            } else {
                assert_eq!(abandoned, vec![(delivery.event.id, Some(step))]);
            }
        }

        let failed = graph.node(step).expect("step");
        assert_eq!(failed.status, NodeStatus::Failed);
        let last = graph.events(None, 100).expect("events");
        let last = last.last().expect("an event");
        assert_eq!((last.kind, last.node), (EventKind::NodeFailed, Some(step)));
        assert_eq!(
            inbox.status_of(ready.expect("leased")).expect("status"),
            Some("failed".into())
        );
        assert!(
            inbox
                .lease(role.events(), ProcessId(9), false, MAX_ATTEMPTS)
                .expect("lease")
                .is_none(),
            "neither the given-up event nor anything else is delivered"
        );
    }

    /// Remembers the deliveries acknowledged, with their reports, and the
    /// ones abandoned (with no report).
    #[derive(Default)]
    struct Acks(Mutex<Vec<(EventId, Option<String>)>>);

    impl ProcessObserver for Acks {
        fn observe(&self, activity: &ProcessActivity<'_>) {
            let seen = if let ProcessActivity::Reacted { delivery, report } = activity {
                (delivery.event.id, Some(report.summary()))
            } else if let ProcessActivity::Abandoned { delivery, .. } = activity {
                (delivery.event.id, None)
            } else {
                return;
            };
            self.0.lock().expect("lock").push(seen);
        }
    }

    #[test]
    fn an_abandoned_delivery_whose_step_was_settled_is_acknowledged_as_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let mut graph = Graph::open(&db).expect("graph");
        let origin = graph
            .create_origin("o", Some("main"), "abc")
            .expect("origin");
        let goal = graph.add_goal("g", "g", false, &[]).expect("goal");
        let question = graph
            .propose_investigation("q", "is it so?", &[origin], &[goal])
            .expect("investigation");
        graph.plan_step(question, 1).expect("plan"); // node.ready
        let role = Handler::Verification;
        let mut inbox = Inbox::open(&db, "verifier").expect("inbox");
        let mut last = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let delivery = inbox
                .lease(role.events(), ProcessId(attempt), false, MAX_ATTEMPTS)
                .expect("lease")
                .expect("node.ready");
            if attempt == MAX_ATTEMPTS {
                // The last holder records its finding, then dies before
                // acknowledging the event.
                assert!(graph.claim(question, &delivery.token).expect("claim"));
                graph
                    .record_finding(question, &delivery.token, "abc", Answer::Yes, "seen it")
                    .expect("finding");
            }
            store::open(&db)
                .expect("db")
                .execute(
                    &format!("UPDATE deliveries SET leased_until = {EPOCH} - 1"),
                    [],
                )
                .expect("expire");
            last = Some(delivery);
        }
        let last = last.expect("leased");
        let answered = graph.node(question).expect("node");
        let before = graph.last_event().expect("last");

        let acks = Acks::default();
        give_up_abandoned(role, role.events(), None, &inbox, &mut graph, &acks).expect("give up");
        let seen = acks.0.into_inner().expect("lock");
        assert_eq!(seen.len(), 1, "acknowledged once, nothing given up");
        assert_eq!(seen[0].0, last.event.id);
        let report = seen[0].1.as_deref().expect("acknowledged, not given up");
        assert!(report.starts_with("ignored: "), "{report}");
        assert_eq!(
            inbox.status_of(last.event.id).expect("status"),
            Some("done".into())
        );
        assert_eq!(graph.node(question).expect("node"), answered);
        assert_eq!(graph.last_event().expect("last"), before, "no node.failed");
    }

    #[test]
    fn a_routed_away_delivery_abandoned_leaves_the_owners_step_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let mut graph = Graph::open(&db).expect("graph");
        let origin = graph
            .create_origin("o", Some("main"), "abc")
            .expect("origin");
        let goal = graph.add_goal("g", "g", false, &[]).expect("goal");
        let step = graph
            .propose_step("s", "s", &[origin], &[goal])
            .expect("step");
        graph.plan_step(step, 1).expect("plan"); // node.ready
        let role = Handler::Verification;
        // The owner, `fast`, is carrying the step out.
        let mut fast = Inbox::open(&db, "fast").expect("inbox");
        let owner = fast
            .lease(role.events(), ProcessId(9), false, MAX_ATTEMPTS)
            .expect("lease")
            .expect("node.ready");
        assert!(graph.claim(step, &owner.token).expect("claim"));
        let mut slow = Inbox::open(&db, "slow").expect("inbox");
        for attempt in 1..=MAX_ATTEMPTS {
            let delivery = slow
                .lease(role.events(), ProcessId(attempt), false, MAX_ATTEMPTS)
                .expect("lease")
                .expect("node.ready");
            slow.assign(role.as_str(), delivery.event.id, "fast")
                .expect("assign");
            // `slow`'s holders keep dying before acknowledging it as routed.
            store::open(&db)
                .expect("db")
                .execute(
                    &format!(
                        "UPDATE deliveries SET leased_until = {EPOCH} - 1 WHERE agent = 'slow'"
                    ),
                    [],
                )
                .expect("expire");
        }
        let recorder = Recorder::default();
        give_up_abandoned(
            role,
            role.events(),
            Some("slow"),
            &slow,
            &mut graph,
            &recorder,
        )
        .expect("give up");
        let abandoned = recorder.0.into_inner().expect("lock");
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].1, None, "nothing of `fast`'s was failed");
        assert_eq!(graph.node(step).expect("step").status, NodeStatus::Running);
    }

    #[test]
    fn random_below_stays_in_bounds_and_spreads() {
        let mut seen = [0usize; 4];
        for _ in 0..400 {
            seen[random_below(4)] += 1;
        }
        assert!(seen.iter().all(|&n| n > 0), "{seen:?}");
        assert_eq!(random_below(1), 0);
    }
}
