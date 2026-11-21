//! [`Process`] — one running agent: lease an event, react, acknowledge.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::agent::Agent;
use crate::event::{Delivery, Inbox};
use crate::graph::{Graph, GraphError};
use crate::process::{HEARTBEAT_SECONDS, Interrupt, ProcessId, ProcessRecord, Processes};
use crate::role::{Context, Reaction, Report, RoleError};
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
    /// Leasing is paused (e.g. too many steps pending).
    Paused(&'a str),
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
    /// The reaction failed.
    Failed {
        /// The delivery.
        delivery: &'a Delivery,
        /// What went wrong.
        error: &'a RoleError,
        /// Whether the event will be delivered again.
        retry: bool,
    },
    /// The process is exiting.
    Exiting(&'a str),
}

/// Receives [`ProcessActivity`] as it happens.
pub trait ProcessObserver: Sync {
    /// Observe one activity.
    fn observe(&self, activity: &ProcessActivity<'_>);
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
    /// exited with the error.
    pub fn run(
        &self,
        options: ProcessOptions,
        observer: &dyn ProcessObserver,
    ) -> Result<String, ProcessError> {
        let project = self.context.project();
        let processes = Processes::open(&project.database_path(), &project.processes_dir())?;
        processes.start(self.id, std::process::id())?;
        observer.observe(&ProcessActivity::Started(&processes.get(self.id)?));

        let finished = AtomicBool::new(false);
        let result = thread::scope(|scope| {
            scope.spawn(|| self.beat(&finished));
            let result = self.serve(&processes, options, observer);
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

    /// Heartbeat: prove liveness, renew held leases, and turn a recorded
    /// stop request into an interrupt.
    fn beat(&self, finished: &AtomicBool) {
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
                // A failed renewal only shortens the lease; the next beat retries.
                inbox.renew(self.id).ok();
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
            if let Some(reason) = role.pause(self.agent, &graph)? {
                processes.set_activity(self.id, Some(&reason))?;
                observer.observe(&ProcessActivity::Paused(&reason));
                if options.exit_when_idle {
                    return Ok(format!("idle ({reason})"));
                }
                thread::sleep(POLL * 4);
                continue;
            }
            let Some(delivery) = inbox.lease(role.reactions(), self.id, role.is_exclusive())?
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
                    && revisit_at_random(&mut graph)?
                {
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
            processes.set_activity(self.id, Some(&format!("reacting to {}", delivery.event)))?;
            observer.observe(&ProcessActivity::Reacting(&delivery));

            let covered = graph.last_event()?;
            let reaction = Reaction::new(
                self.context,
                self.agent,
                self.id,
                &delivery,
                &self.interrupt,
            );
            match role.react(&reaction, &mut graph) {
                Ok(report) => {
                    if role.coalesces()
                        && let Some(through) = covered
                    {
                        let note = format!("covered by the reaction to {}", delivery.event.id);
                        inbox.ack_through(role.reactions(), through, &note)?;
                    }
                    inbox.ack(delivery.event.id, Some(&report.summary()))?;
                    observer.observe(&ProcessActivity::Reacted {
                        delivery: &delivery,
                        report: &report,
                    });
                }
                Err(error) if self.interrupt.is_raised() => {
                    inbox.release(delivery.event.id)?;
                    observer.observe(&ProcessActivity::Failed {
                        delivery: &delivery,
                        error: &error,
                        retry: true,
                    });
                    return Ok("stopped".to_owned());
                }
                Err(RoleError::Graph(error)) => {
                    inbox.release(delivery.event.id)?;
                    return Err(error.into());
                }
                Err(error) => {
                    let retry = inbox.fail(delivery.event.id, &error.to_string(), MAX_ATTEMPTS)?;
                    observer.observe(&ProcessActivity::Failed {
                        delivery: &delivery,
                        error: &error,
                        retry,
                    });
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

/// Record `node.revisited` for a random starting point; `false` when
/// there is none (no active goals).
fn revisit_at_random(graph: &mut Graph) -> Result<bool, ProcessError> {
    let points = graph.starting_points()?;
    if points.is_empty() {
        return Ok(false);
    }
    graph.revisit(points[random_below(points.len())].id)?;
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
