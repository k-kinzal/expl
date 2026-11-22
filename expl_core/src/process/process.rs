//! [`Process`] — one running agent: lease an event, react, acknowledge.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::agent::Agent;
use crate::event::{Delivery, Inbox, InboxError};
use crate::graph::{Graph, GraphError, NodeId};
use crate::process::{HEARTBEAT_SECONDS, Interrupt, ProcessId, ProcessRecord, Processes};
use crate::role::{Context, Reaction, Report, Role, RoleError};
use crate::store::StoreError;
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
        /// What the role did.
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
        error: &'a RoleError,
        /// Whether the event will be delivered again.
        retry: bool,
    },
    /// A delivery whose holder died on its last attempt was given up.
    Abandoned {
        /// The delivery, as its last holder leased it.
        delivery: &'a Delivery,
        /// The step its execution left `running`, now failed.
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
type Held = Mutex<Option<(Delivery, Interrupt)>>;

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
    agent: &'a Agent,
    id: ProcessId,
    interrupt: Interrupt,
}

impl<'a> Process<'a> {
    /// The process registered as `id`, running `agent`.
    #[must_use]
    pub fn new(
        context: &'a Context,
        agent: &'a Agent,
        id: ProcessId,
        interrupt: Interrupt,
    ) -> Self {
        Self {
            context,
            agent,
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
        observer.observe(&ProcessActivity::Started(&processes.get(self.id)?));

        let finished = AtomicBool::new(false);
        let held = Held::default();
        let result = thread::scope(|scope| {
            scope.spawn(|| self.beat(&finished, &held));
            let result = self.serve(&processes, &held, options, observer);
            finished.store(true, Ordering::SeqCst);
            result
        });
        let reason = match &result {
            Ok(reason) => reason.clone(),
            Err(error) => format!("error: {error}"),
        };
        observer.observe(&ProcessActivity::Exiting(&reason));
        processes.exit(self.id, &reason)?;
        result
    }

    /// Heartbeat: prove liveness, renew the held lease (interrupting the
    /// reaction when it is lost), and turn a recorded stop request into an
    /// interrupt.
    fn beat(&self, finished: &AtomicBool, held: &Held) {
        let project = self.context.project();
        let database = project.database_path();
        let (Ok(processes), Ok(inbox)) = (
            Processes::open(&database, &project.processes_dir()),
            Inbox::open(&database, self.agent.name()),
        ) else {
            return;
        };
        let tick = Duration::from_millis(100);
        let mut elapsed = Duration::ZERO;
        while !finished.load(Ordering::SeqCst) {
            if elapsed.is_zero() {
                if let Ok(true) = processes.heartbeat(self.id) {
                    self.interrupt.raise();
                }
                // Locked while renewing, so a lease being settled is not
                // renewed (nor reported lost) after it was given up.
                let held = held
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some((delivery, interrupt)) = held.as_ref() {
                    match inbox.renew(delivery) {
                        Err(InboxError::LeaseLost { .. }) => interrupt.lose_lease(),
                        // A failed renewal only shortens the lease; the next
                        // beat retries.
                        Ok(()) | Err(InboxError::Sqlite(_)) => {}
                    }
                }
            }
            thread::sleep(tick);
            elapsed += tick;
            if elapsed >= Duration::from_secs(HEARTBEAT_SECONDS) {
                elapsed = Duration::ZERO;
            }
        }
    }

    fn serve(
        &self,
        processes: &Processes,
        held: &Held,
        options: ProcessOptions,
        observer: &dyn ProcessObserver,
    ) -> Result<String, ProcessError> {
        let database = self.context.project().database_path();
        let mut inbox = Inbox::open(&database, self.agent.name())?;
        let mut graph = Graph::open(&database)?;
        let role = self.agent.role();
        let trunk = Trunk::new(
            self.context.project(),
            self.context.declaration().trunk.as_deref(),
        );
        let mut followed: Option<Instant> = None;
        let mut unfollowed: Option<String> = None;
        let mut waiting = false;
        // Starts the first cooldown: a fresh process reacts to its backlog
        // before it wanders.
        let mut last_reaction = Instant::now();
        loop {
            if self.interrupt.is_raised() {
                return Ok("stopped".to_owned());
            }
            // Every process follows the trunk, so a merge is noticed while
            // any agent runs; the graph records each move only once.
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
            let weighted = self
                .context
                .declaration()
                .shares_with(self.agent.name(), self.agent.name());
            let owner = weighted.then(|| self.agent.name());
            give_up_abandoned(role, owner, &inbox, &mut graph, observer)?;
            let Some(delivery) =
                inbox.lease(role.reactions(), self.id, role.is_exclusive(), MAX_ATTEMPTS)?
            else {
                if options.exit_when_idle {
                    return Ok("idle".to_owned());
                }
                // Nothing to react to: after its cooldown, an exploration
                // explores again from a starting point picked at random,
                // explored before or not.
                if role
                    .revisit_cooldown(self.agent)
                    .is_some_and(|cooldown| last_reaction.elapsed() >= cooldown)
                    && revisit_at_random(
                        &mut graph,
                        self.agent.declaration().speculate == Some(true),
                    )?
                {
                    // The cooldown runs from the revisit, wherever routing
                    // sends it, so a role revisits once per cooldown.
                    last_reaction = Instant::now();
                    waiting = false;
                    continue;
                }
                if !waiting {
                    processes.set_activity(self.id, Some("waiting for events"))?;
                    waiting = true;
                }
                thread::sleep(POLL);
                continue;
            };
            waiting = false;
            // Agents sharing a role by weight react only to the events
            // assigned to them; the others are theirs to acknowledge. The
            // assignment is recorded before either, so processes that
            // loaded different weights still agree on the one recipient.
            let declaration = self.context.declaration();
            let owner = match declaration.route(self.agent.name(), delivery.event.id) {
                Some(proposed) => {
                    let owner = inbox.assign(role, delivery.event.id, proposed)?;
                    // Assigned to an agent no longer declared for the role
                    // (by a process that loaded an older declaration): it
                    // would never be handled, so move it.
                    if owner != proposed && !declaration.shares_with(self.agent.name(), &owner) {
                        Some(inbox.reassign(role, delivery.event.id, &owner, proposed)?)
                    } else {
                        Some(owner)
                    }
                }
                None => None,
            };
            if let Some(owner) = owner.filter(|owner| owner != self.agent.name()) {
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
                continue;
            }
            processes.set_activity(self.id, Some(&format!("reacting to {}", delivery.event)))?;
            observer.observe(&ProcessActivity::Reacting(&delivery));

            let covered = graph.last_event()?;
            let interrupt = self.interrupt.for_reaction();
            *held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((delivery.clone(), interrupt.clone()));
            let reaction = Reaction::new(self.context, self.agent, self.id, &delivery, &interrupt);
            let result = role.react(&reaction, &mut graph);
            held.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let lost = || observer.observe(&ProcessActivity::LeaseLost(&delivery));
            match result {
                _ if interrupt.lease_lost() => lost(),
                Err(RoleError::Graph(GraphError::LeaseLost(_))) => lost(),
                Ok(report) => {
                    let coalesced = match report.covered().or(covered) {
                        Some(through) if role.coalesces() => {
                            let note = format!("covered by the reaction to {}", delivery.event.id);
                            holding(inbox.ack_through(&delivery, role.reactions(), through, &note))?
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
                Err(RoleError::Graph(error)) if error.is_busy() => {
                    let error = RoleError::Graph(error);
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
                // Any other failure, a graph error included (often a race
                // with another process, such as a step it just settled), is
                // counted and retried; the last attempt fails the step.
                Err(error) => {
                    let why = error.to_string();
                    // The last attempt: clean up before the delivery is
                    // settled, so dying in between leaves it to be retried
                    // or abandoned rather than lost.
                    if delivery.attempts >= MAX_ATTEMPTS {
                        role.abandon(&delivery, &mut graph, &why)?;
                    }
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
            last_reaction = Instant::now();
            processes.set_activity(self.id, None)?;
            if options.once {
                return Ok("reacted to one event".to_owned());
            }
        }
    }
}

/// Give up every delivery of `role` whose holder died on its last attempt,
/// failing first what its reaction left `running`. An agent sharing its role
/// by weight (`weighted_as`, its name) fails only what it was assigned: an
/// event assigned to another agent is that agent's to carry out, whatever
/// became of this agent's delivery of it.
fn give_up_abandoned(
    role: Role,
    weighted_as: Option<&str>,
    inbox: &Inbox,
    graph: &mut Graph,
    observer: &dyn ProcessObserver,
) -> Result<(), ProcessError> {
    for delivery in inbox.abandoned(role.reactions(), MAX_ATTEMPTS)? {
        let why = format!(
            "abandoned: the process reacting to {} died on attempt {} of {MAX_ATTEMPTS}",
            delivery.event, delivery.attempts
        );
        let ours = match weighted_as {
            None => true,
            Some(agent) => inbox.assignment(role, delivery.event.id)?.as_deref() == Some(agent),
        };
        let failed = if ours {
            role.abandon(&delivery, graph, &why)?
        } else {
            None
        };
        if inbox.give_up_abandoned(&delivery)? {
            observer.observe(&ProcessActivity::Abandoned {
                delivery: &delivery,
                failed,
            });
        }
    }
    Ok(())
}

/// Record `node.revisited` for a random starting point (steps not carried
/// out yet among them when exploring `ahead`); `false` when there is none
/// (no active goals).
fn revisit_at_random(graph: &mut Graph, ahead: bool) -> Result<bool, ProcessError> {
    let points = graph.starting_points(ahead)?;
    if points.is_empty() {
        return Ok(false);
    }
    graph.revisit(points[random_below(points.len())].id, ahead)?;
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
    use super::*;
    use crate::event::{EventId, EventKind};
    use crate::graph::NodeStatus;
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
    fn an_execution_whose_holders_keep_dying_fails_its_step() {
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
        let role = Role::Execution;
        let mut inbox = Inbox::open(&db, "executor").expect("inbox");
        let mut ready = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let delivery = inbox
                .lease(role.reactions(), ProcessId(attempt), false, MAX_ATTEMPTS)
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
            give_up_abandoned(role, None, &inbox, &mut graph, &recorder).expect("give up");
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
                .lease(role.reactions(), ProcessId(9), false, MAX_ATTEMPTS)
                .expect("lease")
                .is_none(),
            "neither the given-up event nor anything else is delivered"
        );
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
        let role = Role::Execution;
        // The owner, `fast`, is carrying the step out.
        let mut fast = Inbox::open(&db, "fast").expect("inbox");
        let owner = fast
            .lease(role.reactions(), ProcessId(9), false, MAX_ATTEMPTS)
            .expect("lease")
            .expect("node.ready");
        assert!(graph.claim(step, &owner.token).expect("claim"));
        let mut slow = Inbox::open(&db, "slow").expect("inbox");
        for attempt in 1..=MAX_ATTEMPTS {
            let delivery = slow
                .lease(role.reactions(), ProcessId(attempt), false, MAX_ATTEMPTS)
                .expect("lease")
                .expect("node.ready");
            slow.assign(role, delivery.event.id, "fast")
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
        give_up_abandoned(role, Some("slow"), &slow, &mut graph, &recorder).expect("give up");
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
