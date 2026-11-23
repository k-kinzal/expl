//! [`Worker`] — one consumer's delivery loop: lease an event, execute the
//! registered command, settle the delivery from its outcome.

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use opentelemetry::Context;
use rusqlite::{Connection, params};
use thiserror::Error;

use crate::Interrupt;
use crate::agent::supervisor::{End, Reaped, Stop, TERMINATION_GRACE, reap_group, wait_stopping};
use crate::capacity::Capacity;
use crate::cluster::{self, Authority, CommandSpec};
use crate::delivery::{Delivery, Inbox, InboxError, Lease};
use crate::event::{EPOCH, EventId, EventLog};
use crate::process::{HEARTBEAT_SECONDS, ProcessId, ProcessOptions, ProcessRecord, Processes};
use crate::reaction::{self, Mode, Outcome, Reaction};
use crate::registration::{ConsumerRegistration, Placement, Registration, TaskRegistration};
use crate::store::{self, StoreError};
use crate::telemetry;
use crate::worker::heartbeat::{Active, with_heartbeat};
use crate::worker::telemetry::{Phase, WorkerTelemetry};

/// How long an idle worker waits before looking at its inbox again.
const POLL: Duration = Duration::from_millis(500);

/// How often a failing event is delivered before it is given up.
pub const MAX_ATTEMPTS: i64 = 3;

/// How long a reaction's command gets to exit once it is told to stop
/// (SIGTERM, to the command alone). It has an agent of its own to stop
/// first, which takes that agent's termination grace, the grace of what the
/// agent left behind, and the time a killed process group takes to vanish.
pub const REACTION_GRACE: Duration = Duration::from_secs(30);

/// The name idle commands are logged under.
const IDLE: &str = "idle";

/// The worker could not keep running.
#[derive(Debug, Error)]
pub enum WorkerError {
    /// The database could not be opened.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The database failed.
    #[error("process database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A reaction slot could not be opened or locked.
    #[error("cannot use reaction slots: {0}")]
    Capacity(#[from] io::Error),
    /// The registration names no such consumer.
    #[error("no consumer named `{0}` is registered")]
    UnknownConsumer(String),
    /// The consumer registers nothing to execute for an event.
    #[error("consumer `{0}` registers no command to run")]
    NoCommand(String),
    /// The record was no longer there to adopt: it already started,
    /// exited, or missed its startup deadline (and its replica slot may
    /// have gone to a replacement).
    #[error(
        "process {0} cannot be adopted: it is no longer a created record within its startup \
         deadline"
    )]
    NotAdoptable(ProcessId),
}

/// What a worker is doing, for observers (a CLI writes these to the
/// process log).
#[derive(Debug)]
pub enum Activity<'a> {
    /// The worker adopted its record.
    Started(&'a ProcessRecord),
    /// A periodic task or the idle command said something new.
    Task {
        /// The task's registered name (`idle` for the idle command).
        name: &'a str,
        /// One line of what it printed.
        line: &'a str,
    },
    /// An event was leased.
    Reacting(&'a Delivery),
    /// The command reacted and the event was acknowledged.
    Reacted {
        /// The delivery.
        delivery: &'a Delivery,
        /// What the command reported having done.
        note: Option<&'a str>,
        /// What kind of acknowledgement the command called it.
        kind: Option<&'a str>,
    },
    /// The worker lost its lease on the event (it expired and another
    /// worker may have taken the event over): the command was stopped or
    /// its outcome discarded, and the event was left to the new holder.
    LeaseLost(&'a Delivery),
    /// The reaction failed.
    Failed {
        /// The delivery.
        delivery: &'a Delivery,
        /// What went wrong.
        error: &'a str,
        /// Whether the event will be delivered again.
        retry: bool,
    },
    /// A delivery whose holder died on its last attempt was given up.
    Abandoned {
        /// The delivery, as its last holder leased it.
        delivery: &'a Delivery,
        /// What the command cleaned up after it.
        note: Option<&'a str>,
    },
    /// The worker is exiting.
    Exiting(&'a str),
}

/// Receives [`Activity`] as it happens.
pub trait Observer: Sync {
    /// Observe one activity.
    fn observe(&self, activity: &Activity<'_>);
}

/// The lease a worker is reacting under, shared with its heartbeat.
type Held = Active<Delivery>;

/// `Ok(None)` when the lease was lost.
fn holding<T>(result: Result<T, InboxError>) -> Result<Option<T>, WorkerError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(InboxError::LeaseLost { .. }) => Ok(None),
        Err(InboxError::Sqlite(error)) => Err(error.into()),
    }
}

/// How a command ended.
#[derive(Debug)]
enum Ended {
    /// It exited by itself.
    Exited(ExitStatus),
    /// It was stopped because the worker, or the reaction, was interrupted.
    Interrupted,
    /// It could not be run or supervised.
    Failed(String),
}

/// A command that was executed.
#[derive(Debug)]
struct Ran {
    end: Ended,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn failed(reason: impl Into<String>) -> Self {
        Self {
            end: Ended::Failed(reason.into()),
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    /// The last lines the command printed, for an error message.
    fn tail(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let lines: Vec<&str> = text.trim_end().lines().collect();
        lines[lines.len().saturating_sub(8)..].join("\n")
    }

    /// What a command that reported no outcome came to.
    fn outcome(&self) -> Outcome {
        match &self.end {
            Ended::Exited(status) if status.success() => Outcome::Acknowledge {
                note: None,
                covered: None,
                kind: None,
            },
            Ended::Exited(status) => {
                let tail = self.tail();
                Outcome::Retry {
                    note: if tail.is_empty() {
                        format!("the command exited with {status}")
                    } else {
                        format!("the command exited with {status}: {tail}")
                    },
                }
            }
            Ended::Interrupted => Outcome::Release {
                note: Some("the reaction was interrupted".to_owned()),
            },
            Ended::Failed(reason) => Outcome::Retry {
                note: reason.clone(),
            },
        }
    }
}

/// One worker of a consumer. It leases the consumer's events one at a time,
/// executes the registered command for each, and settles the delivery from
/// the command's [`Outcome`]; between events it runs the explorer's
/// periodic tasks and, when idle long enough, the consumer's idle command.
#[derive(Debug, Clone)]
pub struct Worker<'a> {
    /// Where the explorer's commands run.
    pub directory: &'a Path,
    /// The explorer's database.
    pub database: &'a Path,
    /// The explorer's state directory: its reaction slots live here.
    pub state: &'a Path,
    /// Where process logs go.
    pub logs: &'a Path,
    /// What the explorer registered.
    pub registration: &'a Registration,
    /// The consumer this worker serves.
    pub consumer: &'a str,
    /// The worker's process record.
    pub id: ProcessId,
    /// Raised when the worker must stop.
    pub interrupt: Interrupt,
    /// The elected daemon the worker runs under, if any.
    pub authority: Option<&'a Authority>,
}

impl Worker<'_> {
    /// Adopt the record, react to events until stopped (or as `options`
    /// say), and record the exit. Returns the exit reason.
    ///
    /// # Errors
    ///
    /// [`WorkerError`] when the database fails; the record is marked exited
    /// with the error. [`WorkerError::NotAdoptable`] when the record cannot
    /// be adopted; it is left as it is.
    pub fn run(
        &self,
        options: ProcessOptions,
        observer: &dyn Observer,
    ) -> Result<String, WorkerError> {
        let consumer = self
            .registration
            .consumer
            .get(self.consumer)
            .ok_or_else(|| WorkerError::UnknownConsumer(self.consumer.to_owned()))?;
        if consumer.run.is_empty() {
            return Err(WorkerError::NoCommand(self.consumer.to_owned()));
        }
        let processes = Processes::open(self.database, self.logs)?;
        if !processes.start(self.id, std::process::id())? {
            return Err(WorkerError::NotAdoptable(self.id));
        }
        let record = processes.get(self.id)?;
        let telemetry = WorkerTelemetry::new(observer, self.consumer, consumer, &record);
        telemetry.observe(&Activity::Started(&record));

        let mut heartbeat = None;
        let result = with_heartbeat(
            Duration::from_secs(HEARTBEAT_SECONDS),
            move |held| {
                if self.orphaned() {
                    self.interrupt.raise();
                }
                if heartbeat.is_none() {
                    heartbeat = Processes::open(self.database, self.logs)
                        .ok()
                        .zip(Inbox::open(self.database, self.consumer).ok());
                }
                if let Some((processes, inbox)) = &heartbeat {
                    self.beat(processes, inbox, held);
                }
            },
            |held| self.serve(&processes, consumer, held, options, &telemetry),
        );
        let reason = match &result {
            Ok(reason) => reason.clone(),
            Err(error) => format!("error: {error}"),
        };
        telemetry.observe(&Activity::Exiting(&reason));
        if self.authority.is_none_or(Authority::is_live) {
            processes.exit(self.id, &reason)?;
        }
        result
    }

    /// Whether the daemon this worker was started under is gone: its term
    /// ended, so whatever it started must stop.
    fn orphaned(&self) -> bool {
        self.authority.is_some_and(|authority| !authority.is_live())
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
        consumer: &ConsumerRegistration,
        held: &Held,
        options: ProcessOptions,
        observer: &WorkerTelemetry<'_>,
    ) -> Result<String, WorkerError> {
        let route = self.registration.route_for(self.consumer);
        if route
            .as_ref()
            .is_some_and(|route| !route.members().contains(self.consumer))
        {
            return Ok("consumer is not reachable through its delivery group".to_owned());
        }
        let capacity = Capacity::new(
            self.state,
            self.consumer,
            consumer.concurrency,
            route.as_ref(),
        )?;
        let shared_capacity = route
            .as_ref()
            .is_some_and(|route| route.groups(self.consumer).next().is_some());
        let mut inbox = Inbox::open(self.database, self.consumer)?;
        let conn = store::open(self.database)?;
        let mut tasks = Tasks::new(&conn, &self.registration.task);
        // Advances with every command run for a delivery, from a start
        // that differs between workers: commands that take turns among
        // alternatives spread over them, within a worker and across replicas.
        let turn = AtomicU64::new(u64::from(std::process::id()));
        let mut activity = "";
        // Starts the first cooldown: a fresh worker reacts to its backlog
        // before its idle command runs.
        let mut last_reaction = Instant::now();
        observer.state(Phase::WaitingForEvents);
        loop {
            if self.orphaned() {
                self.interrupt.raise();
            }
            if self.interrupt.is_raised() {
                return Ok("stopped".to_owned());
            }
            for (name, task) in tasks.due()? {
                let ran = self.run_command(
                    &mut self.command(&task.run, &BTreeMap::new()),
                    false,
                    Stop::Group(TERMINATION_GRACE),
                    &self.interrupt,
                );
                tasks.say(name, &ran, observer);
            }
            self.give_up_abandoned(consumer, &inbox, &turn, observer)?;
            let (delivery, slots) = match inbox.lease_with(
                &consumer.on,
                self.id.0,
                consumer.exclusive,
                MAX_ATTEMPTS,
                route.as_ref(),
                || capacity.try_acquire().map_err(WorkerError::from),
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
                    // An idle consumer can produce an event of its own.
                    if let Some(idle) = &consumer.idle
                        && last_reaction.elapsed() >= Duration::from_secs(idle.after)
                    {
                        let ran = self.run_command(
                            self.command(&idle.run, &consumer.env)
                                .env(reaction::CONSUMER, self.consumer),
                            false,
                            Stop::Group(TERMINATION_GRACE),
                            &self.interrupt,
                        );
                        tasks.say(IDLE, &ran, observer);
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
            processes.set_activity(self.id, Some(&format!("reacting to {}", delivery.event)))?;
            activity = "";
            observer.observe(&Activity::Reacting(&delivery));

            let covered = EventLog::last(&conn)?;
            let interrupt = self.interrupt.for_reaction();
            let (reported, outcome) = held.react(delivery.clone(), &interrupt, || {
                self.execute(consumer, &delivery, Mode::React, &turn, &interrupt)
            });
            let lost = || observer.observe(&Activity::LeaseLost(&delivery));
            let failed = |error: &str, retry: bool| {
                observer.observe(&Activity::Failed {
                    delivery: &delivery,
                    error,
                    retry,
                });
            };
            // A command that failed because the daemon's term ended did not
            // fail at its event: the worker stops and gives the event back,
            // rather than spend the event's attempts before its heartbeat
            // notices.
            if self.orphaned() {
                self.interrupt.raise();
            }
            let stopping = self.interrupt.is_raised();
            match outcome {
                _ if interrupt.lease_lost() => lost(),
                Outcome::Lost => lost(),
                Outcome::Acknowledge {
                    note,
                    covered: through,
                    kind,
                } => {
                    let through = through.map(EventId).or(covered);
                    let coalesced = match through {
                        Some(through) if consumer.coalesce => {
                            let note = format!("covered by the reaction to {}", delivery.event.id);
                            holding(inbox.ack_through(&delivery, &consumer.on, through, &note))?
                                .is_some()
                        }
                        Some(_) | None => true,
                    };
                    if coalesced && holding(inbox.ack(&delivery, note.as_deref()))?.is_some() {
                        observer.observe(&Activity::Reacted {
                            delivery: &delivery,
                            note: note.as_deref(),
                            kind: kind.as_deref(),
                        });
                    } else {
                        lost();
                    }
                }
                // Transient (a busy database, say), or the worker is
                // stopping: give the event back; the attempt does not count.
                Outcome::Release { note } => {
                    let note = note.unwrap_or_else(|| "the reaction was released".to_owned());
                    if holding(inbox.release(&delivery))?.is_some() {
                        failed(&note, true);
                    } else {
                        lost();
                    }
                    if stopping {
                        return Ok("stopped".to_owned());
                    }
                    thread::sleep(POLL * 4);
                }
                // The worker is stopping: whatever the command made of being
                // stopped, its event goes back undelivered.
                Outcome::Retry { note } if stopping => {
                    if holding(inbox.release(&delivery))?.is_some() {
                        failed(&note, true);
                    } else {
                        lost();
                    }
                    return Ok("stopped".to_owned());
                }
                // Counted and retried; the last attempt gives the event up.
                // A command that reported the failure itself has cleaned up
                // after its last attempt; for one that died, ask now.
                Outcome::Retry { note } => {
                    let settled = !reported
                        && delivery.attempts >= MAX_ATTEMPTS
                        && self.settle_abandoned(consumer, &inbox, &delivery, &turn, observer)?;
                    if !settled {
                        match holding(inbox.fail(&delivery, &note, MAX_ATTEMPTS))? {
                            Some(retry) => failed(&note, retry),
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

    /// Give up every delivery whose holder died on its last attempt, after
    /// the consumer's command has cleaned up what its reaction left behind
    /// (run with [`Mode::Abandon`]; a retry it reports says what it cleaned
    /// up). A delivery the command acknowledges instead (its work was
    /// settled elsewhere meanwhile) is acknowledged: nothing of it was left
    /// hanging.
    fn give_up_abandoned(
        &self,
        consumer: &ConsumerRegistration,
        inbox: &Inbox,
        turn: &AtomicU64,
        observer: &dyn Observer,
    ) -> Result<(), WorkerError> {
        for delivery in inbox.abandoned(&consumer.on, MAX_ATTEMPTS)? {
            if self.interrupt.is_raised() {
                break;
            }
            let (reported, outcome) =
                self.execute(consumer, &delivery, Mode::Abandon, turn, &self.interrupt);
            let note = match outcome {
                // Only a command that says so: one that merely exits with
                // success has not looked at what was left behind.
                Outcome::Acknowledge { note, kind, .. } if reported => {
                    if holding(inbox.ack(&delivery, note.as_deref()))?.is_some() {
                        observer.observe(&Activity::Reacted {
                            delivery: &delivery,
                            note: note.as_deref(),
                            kind: kind.as_deref(),
                        });
                        continue;
                    }
                    None
                }
                Outcome::Retry { note } if reported => Some(note),
                Outcome::Acknowledge { .. }
                | Outcome::Retry { .. }
                | Outcome::Release { .. }
                | Outcome::Lost => None,
            };
            if inbox.give_up_abandoned(&delivery)? {
                observer.observe(&Activity::Abandoned {
                    delivery: &delivery,
                    note: note.as_deref(),
                });
            }
        }
        Ok(())
    }

    /// A reaction died on its last attempt without reporting: let the
    /// command clean up after it. `true` when the command acknowledged the
    /// delivery instead and it was settled so.
    fn settle_abandoned(
        &self,
        consumer: &ConsumerRegistration,
        inbox: &Inbox,
        delivery: &Delivery,
        turn: &AtomicU64,
        observer: &dyn Observer,
    ) -> Result<bool, WorkerError> {
        let (reported, outcome) =
            self.execute(consumer, delivery, Mode::Abandon, turn, &self.interrupt);
        if reported
            && let Outcome::Acknowledge { note, kind, .. } = outcome
            && holding(inbox.ack(delivery, note.as_deref()))?.is_some()
        {
            observer.observe(&Activity::Reacted {
                delivery,
                note: note.as_deref(),
                kind: kind.as_deref(),
            });
            return Ok(true);
        }
        Ok(false)
    }

    /// Execute the consumer's command for `delivery`. Returns what it came
    /// to, and whether the command reported that itself.
    fn execute(
        &self,
        consumer: &ConsumerRegistration,
        delivery: &Delivery,
        mode: Mode,
        turn: &AtomicU64,
        interrupt: &Interrupt,
    ) -> (bool, Outcome) {
        let remote = consumer.placement == Placement::Any;
        // The outcome file must be where the command runs: under the
        // cluster's shared directory when that is another member.
        let scratch = match self.authority.filter(|_| remote) {
            Some(authority) => tempfile::Builder::new()
                .prefix("expl-reaction-")
                .tempdir_in(authority.cluster().root()),
            None => tempfile::Builder::new().prefix("expl-reaction-").tempdir(),
        };
        let scratch = match scratch {
            Ok(scratch) => scratch,
            Err(error) => {
                return (
                    false,
                    Outcome::Retry {
                        note: format!("cannot make room for the reaction's outcome: {error}"),
                    },
                );
            }
        };
        let path = scratch.path().join("outcome.json");
        let reaction = Reaction {
            consumer: self.consumer.to_owned(),
            delivery: delivery.clone(),
            max_attempts: MAX_ATTEMPTS,
            worker: self.id.0,
            turn: turn.fetch_add(1, Ordering::Relaxed),
            mode,
            outcome: Some(path.clone()),
        };
        let mut command = self.command(&consumer.run, &consumer.env);
        command.envs(reaction.environment());
        if reaction.delivery.event.subject.is_none() {
            command.env_remove(reaction::SUBJECT);
        }
        // The command's own spans continue the delivery's trace.
        match telemetry::trace_context(&Context::current()) {
            Some((parent, state)) => {
                command.env(reaction::TRACEPARENT, parent);
                match state {
                    Some(state) => command.env(reaction::TRACESTATE, state),
                    None => command.env_remove(reaction::TRACESTATE),
                };
            }
            None => {
                command
                    .env_remove(reaction::TRACEPARENT)
                    .env_remove(reaction::TRACESTATE);
            }
        }
        // The command is told to stop and left to stop what it started: a
        // signal to its whole group would kill a git command or an agent
        // under way, and the reaction would take that for a failure.
        let ran = self.run_command(
            &mut command,
            remote,
            Stop::Leader(REACTION_GRACE),
            interrupt,
        );
        match Outcome::read(&path) {
            Ok(Some(outcome)) => (true, outcome),
            Ok(None) => (false, ran.outcome()),
            Err(error) => (
                false,
                Outcome::Retry {
                    note: format!("the command's outcome is unreadable: {error}"),
                },
            ),
        }
    }

    /// `argv` as a command in the explorer's directory.
    fn command(&self, argv: &[String], env: &BTreeMap<String, String>) -> Command {
        let mut command = Command::new(argv.first().map_or("", String::as_str));
        command
            .args(argv.iter().skip(1))
            .current_dir(self.directory)
            .envs(env);
        command
    }

    /// Run `command` to its end, or stop it when `interrupt` is raised,
    /// as `stop` says: on a member of the cluster when `remote`
    /// under a daemon, else as a local child.
    fn run_command(
        &self,
        command: &mut Command,
        remote: bool,
        stop: Stop,
        interrupt: &Interrupt,
    ) -> Ran {
        if command.get_program().is_empty() {
            return Ran::failed("no command to run");
        }
        match self.authority.filter(|_| remote) {
            Some(authority) => run_on_member(authority, command, self.directory, stop, interrupt),
            None => run_locally(command, stop, interrupt).unwrap_or_else(|error| {
                Ran::failed(format!(
                    "cannot run `{}`: {error}",
                    command.get_program().display()
                ))
            }),
        }
    }
}

fn run_on_member(
    authority: &Authority,
    command: &Command,
    directory: &Path,
    stop: Stop,
    interrupt: &Interrupt,
) -> Ran {
    let spec = CommandSpec::capture(command, directory);
    let spec = match stop {
        Stop::Leader(grace) => spec.stopping_itself_within(grace),
        Stop::Group(_) => spec,
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let end =
        match authority
            .cluster()
            .execute(spec, None, None, interrupt, (&mut stdout, &mut stderr))
        {
            Ok(cluster::Outcome::Exited(status)) => Ended::Exited(ExitStatus::from_raw(status)),
            Ok(cluster::Outcome::Interrupted) => Ended::Interrupted,
            Ok(cluster::Outcome::TimedOut) => Ended::Failed("the command timed out".to_owned()),
            Ok(cluster::Outcome::Lingering { group }) => Ended::Failed(format!(
                "the command's processes in group {group} survived SIGKILL"
            )),
            Ok(cluster::Outcome::Failed(reason)) => Ended::Failed(reason),
            Err(error) => Ended::Failed(format!("cluster transport failed: {error}")),
        };
    Ran {
        end,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

/// Run a local child in its own process group, its output collected from
/// files so that inherited pipes cannot stall it.
fn run_locally(command: &mut Command, stop: Stop, interrupt: &Interrupt) -> io::Result<Ran> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let end = wait_stopping(&mut child, || interrupt.is_raised(), None, stop);
    let reaped = reap_group(child.id());
    let read = |file: &mut std::fs::File| -> io::Result<String> {
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    let end = match end? {
        _ if reaped == Reaped::Lingering => {
            Ended::Failed("the command left processes behind that survived SIGKILL".to_owned())
        }
        End::Exited(status) => Ended::Exited(status),
        End::Interrupted | End::TimedOut => Ended::Interrupted,
    };
    Ok(Ran {
        end,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
    })
}

/// The explorer's periodic tasks, as one worker runs them: each once when
/// the worker starts, then whenever its period has passed and no other
/// worker of the explorer ran it meanwhile.
struct Tasks<'a> {
    conn: &'a Connection,
    registered: &'a BTreeMap<String, TaskRegistration>,
    /// When this worker next looks whether a task is due.
    next: BTreeMap<&'a str, Instant>,
    /// What each task last said, so that it is logged once.
    said: BTreeMap<String, String>,
}

impl<'a> Tasks<'a> {
    fn new(conn: &'a Connection, tasks: &'a BTreeMap<String, TaskRegistration>) -> Self {
        Self {
            conn,
            registered: tasks,
            next: BTreeMap::new(),
            said: BTreeMap::new(),
        }
    }

    /// The tasks this worker is to run now.
    fn due(&mut self) -> Result<Vec<(&'a str, &'a TaskRegistration)>, rusqlite::Error> {
        let mut due = Vec::new();
        let now = Instant::now();
        for (name, task) in self.registered {
            let first = !self.next.contains_key(name.as_str());
            if !first && self.next[name.as_str()] > now {
                continue;
            }
            self.next
                .insert(name.as_str(), now + Duration::from_secs(task.every.max(1)));
            // One row per task holds when it last ran: whoever moves it
            // forward runs the task, so the workers of an explorer run it
            // once per period between them.
            let claimed = self.conn.execute(
                &format!(
                    "INSERT INTO tasks (name, ran_at) VALUES (?1, {EPOCH}) \
                     ON CONFLICT (name) DO UPDATE SET ran_at = {EPOCH} \
                     WHERE ?3 OR tasks.ran_at <= {EPOCH} - ?2"
                ),
                params![name, task.every as i64, first],
            )?;
            if claimed == 1 {
                due.push((name.as_str(), task));
            }
        }
        Ok(due)
    }

    /// Log what the task `name` printed, unless it is what it printed the
    /// last time: a task that keeps reporting the same trouble says it once.
    fn say(&mut self, name: &str, ran: &Ran, observer: &dyn Observer) {
        let output = match &ran.end {
            Ended::Exited(status) if status.success() => ran.stdout.trim().to_owned(),
            Ended::Exited(status) => format!("`{name}` exited with {status}: {}", ran.tail()),
            Ended::Interrupted => return,
            Ended::Failed(reason) => format!("`{name}` failed: {reason}"),
        };
        if self.said.get(name).is_some_and(|said| *said == output) {
            return;
        }
        for line in output.lines().filter(|line| !line.trim().is_empty()) {
            observer.observe(&Activity::Task { name, line });
        }
        self.said.insert(name.to_owned(), output);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::cluster::Cluster;
    use crate::registration::IdleCommand;
    use crate::testing::scaled;

    const CONSUMER: &str = "reactor";
    const KIND: &str = "thing.added";

    /// What a worker did, one line per activity.
    #[derive(Default)]
    struct Seen(Mutex<Vec<String>>);

    impl Observer for Seen {
        fn observe(&self, activity: &Activity<'_>) {
            let line = match activity {
                Activity::Started(_) | Activity::Reacting(_) => return,
                Activity::Task { name, line } => format!("task {name}: {line}"),
                Activity::Reacted {
                    delivery,
                    note,
                    kind,
                } => format!(
                    "reacted {}: {}{}",
                    delivery.event.id,
                    note.unwrap_or("-"),
                    kind.map_or_else(String::new, |kind| format!(" [{kind}]"))
                ),
                Activity::LeaseLost(delivery) => format!("lost {}", delivery.event.id),
                Activity::Failed {
                    delivery,
                    error,
                    retry,
                } => format!(
                    "failed {} ({}): {error}",
                    delivery.event.id,
                    if *retry { "retry" } else { "giving up" }
                ),
                Activity::Abandoned { delivery, note } => {
                    format!("abandoned {}: {}", delivery.event.id, note.unwrap_or("-"))
                }
                Activity::Exiting(reason) => format!("exiting: {reason}"),
            };
            self.0.lock().expect("lock").push(line);
        }
    }

    /// A fresh explorer: a database with the framework's tables and one
    /// consumer of `thing.added` events, whose command is a shell script.
    /// `@DIR@` in a script is the explorer's directory.
    struct Explorer {
        dir: tempfile::TempDir,
        database: PathBuf,
        registration: Registration,
    }

    impl Explorer {
        fn new(script: &str) -> Self {
            let dir = tempfile::tempdir().expect("dir");
            let database = dir.path().join("explorer.db");
            store::initialize(&store::open(&database).expect("open")).expect("tables");
            let mut explorer = Self {
                dir,
                database,
                registration: Registration::default(),
            };
            let run = explorer.shell(script);
            explorer.registration.consumer.insert(
                CONSUMER.to_owned(),
                ConsumerRegistration {
                    on: vec![KIND.to_owned()],
                    run,
                    replicas: 1,
                    ..ConsumerRegistration::default()
                },
            );
            explorer
        }

        fn shell(&self, script: &str) -> Vec<String> {
            let script = script.replace("@DIR@", &self.dir.path().display().to_string());
            vec!["sh".to_owned(), "-c".to_owned(), script]
        }

        fn consumer(&mut self) -> &mut ConsumerRegistration {
            self.registration
                .consumer
                .get_mut(CONSUMER)
                .expect("consumer")
        }

        fn conn(&self) -> Connection {
            store::open(&self.database).expect("open")
        }

        fn publish(&self, subject: Option<i64>) -> EventId {
            EventLog::publish(&self.conn(), KIND, subject).expect("publish")
        }

        fn run(
            &self,
            options: ProcessOptions,
            interrupt: Interrupt,
            authority: Option<&Authority>,
        ) -> (String, Vec<String>) {
            let record = Processes::over(self.conn(), self.dir.path())
                .create(CONSUMER, None, false)
                .expect("record");
            let seen = Seen::default();
            let reason = Worker {
                directory: self.dir.path(),
                database: &self.database,
                state: self.dir.path(),
                logs: self.dir.path(),
                registration: &self.registration,
                consumer: CONSUMER,
                id: record.id,
                interrupt,
                authority,
            }
            .run(options, &seen)
            .expect("worker");
            (reason, seen.0.into_inner().expect("lock"))
        }

        /// One worker that reacts to everything deliverable, then exits.
        fn until_idle(&self) -> Vec<String> {
            let options = ProcessOptions {
                once: false,
                exit_when_idle: true,
            };
            let (reason, seen) = self.run(options, Interrupt::default(), None);
            assert_eq!(reason, "idle");
            seen
        }

        /// The status, attempts and note of the consumer's delivery.
        fn delivery(&self, event: EventId) -> (String, i64, Option<String>) {
            self.conn()
                .query_row(
                    "SELECT status, attempts, note FROM deliveries \
                     WHERE agent = ?1 AND event_id = ?2",
                    params![CONSUMER, event.0],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("delivery")
        }

        fn status(&self, event: EventId) -> String {
            self.delivery(event).0
        }

        /// The lines a script appended to `file` in the explorer's directory.
        fn lines(&self, file: &str) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join(file))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn wait_for(&self, file: &str) {
            let deadline = Instant::now() + scaled(Duration::from_secs(20));
            while self.lines(file).is_empty() {
                assert!(Instant::now() < deadline, "`{file}` never appeared");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn a_command_that_exits_with_success_acknowledges_its_event() {
        let explorer = Explorer::new(concat!(
            r#"echo "$EXPL_CONSUMER $EXPL_EVENT $EXPL_SUBJECT $EXPL_REACTION" "#,
            r#""$EXPL_ATTEMPTS/$EXPL_MAX_ATTEMPTS ${EXPL_LEASE:+leased} $EXPL_WORKER" "#,
            ">> @DIR@/calls",
        ));
        let event = explorer.publish(Some(7));
        let seen = explorer.until_idle();
        assert_eq!(
            explorer.lines("calls"),
            ["reactor thing.added 7 react 1/3 leased 1"]
        );
        assert_eq!(explorer.status(event), "done");
        assert_eq!(seen, ["reacted e1: -", "exiting: idle"]);
        // Nothing is left to deliver to the next worker.
        assert_eq!(explorer.until_idle(), ["exiting: idle"]);
        assert_eq!(explorer.lines("calls").len(), 1);
    }

    #[test]
    fn a_failing_event_is_retried_then_given_up_after_the_command_cleans_up() {
        let explorer = Explorer::new(
            r#"echo "$EXPL_REACTION $EXPL_ATTEMPTS" >> @DIR@/calls; echo boom >&2; exit 1"#,
        );
        let event = explorer.publish(None);
        let seen = explorer.until_idle();
        // The command died on its last attempt without a word: it is run
        // once more, to clean up, before the event is given up.
        assert_eq!(
            explorer.lines("calls"),
            ["react 1", "react 2", "react 3", "abandon 3"]
        );
        let (status, attempts, note) = explorer.delivery(event);
        assert_eq!((status.as_str(), attempts), ("failed", 3));
        assert!(note.expect("note").ends_with("boom"));
        let failures: Vec<&str> = seen
            .iter()
            .map(|line| line.split(':').next().unwrap_or_default())
            .collect();
        assert_eq!(
            failures,
            [
                "failed e1 (retry)",
                "failed e1 (retry)",
                "failed e1 (giving up)",
                "exiting"
            ]
        );
    }

    /// Lease the event to holders that die, as many times as attempts are
    /// allowed.
    fn kill_every_holder(explorer: &Explorer) {
        let mut inbox = Inbox::open(&explorer.database, CONSUMER).expect("inbox");
        for holder in 1..=MAX_ATTEMPTS {
            inbox
                .lease(&[KIND], 100 + holder, false, MAX_ATTEMPTS)
                .expect("lease")
                .expect("delivery");
            explorer
                .conn()
                .execute(
                    &format!("UPDATE deliveries SET leased_until = {EPOCH} - 1"),
                    [],
                )
                .expect("expire");
        }
    }

    #[test]
    fn a_delivery_whose_holders_all_died_is_given_up_once_the_command_cleaned_up() {
        let explorer = Explorer::new(
            r#"echo "$EXPL_REACTION $EXPL_ATTEMPTS" >> @DIR@/calls
               printf '{"outcome":"retry","note":"step 7 failed"}' > "$EXPL_OUTCOME""#,
        );
        let event = explorer.publish(Some(7));
        kill_every_holder(&explorer);
        let seen = explorer.until_idle();
        assert_eq!(explorer.lines("calls"), ["abandon 3"]);
        assert_eq!(seen, ["abandoned e1: step 7 failed", "exiting: idle"]);
        assert_eq!(explorer.status(event), "failed");

        // A command that finds the work settled elsewhere says so, and the
        // delivery is acknowledged instead. Exiting with success alone
        // acknowledges nothing here.
        for (script, status, line) in [
            (
                r#"printf '{"outcome":"acknowledge","note":"ignored: settled"}' > "$EXPL_OUTCOME""#,
                "done",
                "reacted e1: ignored: settled",
            ),
            ("exit 0", "failed", "abandoned e1: -"),
        ] {
            let explorer = Explorer::new(script);
            let event = explorer.publish(Some(7));
            kill_every_holder(&explorer);
            assert_eq!(explorer.until_idle(), [line, "exiting: idle"]);
            assert_eq!(explorer.status(event), status);
        }
    }

    #[test]
    fn stopping_the_worker_gives_its_event_back() {
        let explorer = Explorer::new("echo started >> @DIR@/calls; sleep 30");
        let event = explorer.publish(None);
        let interrupt = Interrupt::default();
        let (reason, seen) = thread::scope(|scope| {
            let worker =
                scope.spawn(|| explorer.run(ProcessOptions::default(), interrupt.clone(), None));
            explorer.wait_for("calls");
            assert_eq!(explorer.status(event), "leased");
            interrupt.raise();
            worker.join().expect("worker")
        });
        assert_eq!(reason, "stopped");
        // The attempt does not count.
        let (status, attempts, _) = explorer.delivery(event);
        assert_eq!((status.as_str(), attempts), ("pending", 0));
        assert_eq!(
            seen,
            [
                "failed e1 (retry): the reaction was interrupted",
                "exiting: stopped"
            ]
        );
    }

    #[test]
    fn a_worker_whose_daemon_is_gone_gives_its_event_back_and_stops() {
        // The command fails as soon as it is let go, as one does that
        // cannot enter a term that has ended.
        let explorer = Explorer::new(concat!(
            "echo started >> @DIR@/calls; ",
            "while [ ! -f @DIR@/go ]; do sleep 0.02; done; exit 1",
        ));
        let event = explorer.publish(None);
        let shared = tempfile::tempdir().expect("cluster");
        let cluster = Cluster::open(shared.path()).expect("cluster");
        let (authority, term) = cluster.test_term("alpha");
        let (reason, seen) = thread::scope(|scope| {
            let worker = scope.spawn(|| {
                explorer.run(
                    ProcessOptions::default(),
                    Interrupt::default(),
                    Some(&authority),
                )
            });
            explorer.wait_for("calls");
            drop(term);
            std::fs::write(explorer.dir.path().join("go"), "").expect("let the command go");
            worker.join().expect("worker")
        });
        assert_eq!(reason, "stopped");
        // The failure was the daemon's, not the event's: no attempt is
        // spent, and the command is not run again by this worker.
        let (status, attempts, _) = explorer.delivery(event);
        assert_eq!((status.as_str(), attempts), ("pending", 0));
        assert_eq!(explorer.lines("calls").len(), 1);
        assert_eq!(seen.last().map(String::as_str), Some("exiting: stopped"));
    }

    #[test]
    fn a_command_told_to_stop_is_left_to_stop_what_it_started() {
        // The command has a child of its own under way. Were the child
        // signalled too, it would say so.
        let explorer = Explorer::new(
            r#"(trap 'echo "child told" >> @DIR@/calls' TERM; while :; do sleep 0.1; done) &
               trap 'echo told >> @DIR@/calls; kill -KILL $!; exit 0' TERM
               echo started >> @DIR@/calls
               wait"#,
        );
        let event = explorer.publish(None);
        let interrupt = Interrupt::default();
        let reason = thread::scope(|scope| {
            let worker =
                scope.spawn(|| explorer.run(ProcessOptions::default(), interrupt.clone(), None));
            explorer.wait_for("calls");
            interrupt.raise();
            worker.join().expect("worker").0
        });
        assert_eq!(reason, "stopped");
        assert_eq!(explorer.lines("calls"), ["started", "told"]);
        assert_eq!(explorer.status(event), "pending");
    }

    #[test]
    fn the_outcome_a_command_reports_settles_its_delivery() {
        let explorer = Explorer::new(
            r#"case "$EXPL_SUBJECT" in
               1) printf '{"outcome":"acknowledge","note":"made one","kind":"special"}' > "$EXPL_OUTCOME" ;;
               2) if [ -f @DIR@/released ]; then
                    printf '{"outcome":"acknowledge","note":"second time"}' > "$EXPL_OUTCOME"
                  else
                    touch @DIR@/released
                    printf '{"outcome":"release","note":"busy"}' > "$EXPL_OUTCOME"
                  fi ;;
               3) printf '{"outcome":"retry","note":"no good"}' > "$EXPL_OUTCOME" ;;
               4) printf '{"outcome":"lost"}' > "$EXPL_OUTCOME" ;;
               esac
               echo "$EXPL_SUBJECT $EXPL_REACTION" >> @DIR@/calls"#,
        );
        let events: Vec<EventId> = (1..=4)
            .map(|subject| explorer.publish(Some(subject)))
            .collect();
        let seen = explorer.until_idle();
        assert_eq!(
            seen,
            [
                "reacted e1: made one [special]",
                // Released: the attempt does not count, and it comes back.
                "failed e2 (retry): busy",
                "reacted e2: second time",
                // A failure the command reported itself is not cleaned up
                // after again.
                "failed e3 (retry): no good",
                "failed e3 (retry): no good",
                "failed e3 (giving up): no good",
                "lost e4",
                "exiting: idle",
            ]
        );
        assert!(
            explorer
                .lines("calls")
                .iter()
                .all(|call| call.ends_with("react"))
        );
        let deliveries: Vec<(String, i64)> = events
            .iter()
            .map(|event| {
                let (status, attempts, _) = explorer.delivery(*event);
                (status, attempts)
            })
            .collect();
        assert_eq!(
            deliveries,
            [
                ("done".to_owned(), 1),
                ("done".to_owned(), 1),
                ("failed".to_owned(), 3),
                // Left to whoever holds it now.
                ("leased".to_owned(), 1),
            ]
        );
        assert_eq!(explorer.delivery(events[0]).2.as_deref(), Some("made one"));
    }

    #[test]
    fn a_coalescing_consumer_covers_its_earlier_events_with_one_reaction() {
        // The command says how far it looked: later events are reacted to.
        let mut explorer = Explorer::new(
            r#"echo "$EXPL_EVENT_ID" >> @DIR@/calls
               if [ "$EXPL_EVENT_ID" = 1 ]; then
                 printf '{"outcome":"acknowledge","note":"planned","covered":2}' > "$EXPL_OUTCOME"
               fi"#,
        );
        explorer.consumer().coalesce = true;
        let events: Vec<EventId> = (0..3).map(|_| explorer.publish(None)).collect();
        explorer.until_idle();
        assert_eq!(explorer.lines("calls"), ["1", "3"]);
        assert_eq!(
            explorer.delivery(events[1]),
            (
                "done".to_owned(),
                0,
                Some("covered by the reaction to e1".to_owned())
            )
        );
        assert_eq!(explorer.status(events[2]), "done");

        // A command that does not say covers what was there when it began.
        let mut explorer = Explorer::new(r#"echo "$EXPL_EVENT_ID" >> @DIR@/calls"#);
        explorer.consumer().coalesce = true;
        let events: Vec<EventId> = (0..3).map(|_| explorer.publish(None)).collect();
        explorer.until_idle();
        assert_eq!(explorer.lines("calls"), ["1"]);
        assert!(events.iter().all(|event| explorer.status(*event) == "done"));

        // Without coalescing, every event has its own reaction, and each
        // reaction the next turn.
        let explorer = Explorer::new(
            r#"echo "$EXPL_EVENT_ID" >> @DIR@/calls; echo "$EXPL_TURN" >> @DIR@/turns"#,
        );
        for _ in 0..3 {
            explorer.publish(None);
        }
        explorer.until_idle();
        assert_eq!(explorer.lines("calls"), ["1", "2", "3"]);
        let turns: Vec<u64> = explorer
            .lines("turns")
            .iter()
            .map(|turn| turn.parse().expect("turn"))
            .collect();
        assert_eq!(turns, [turns[0], turns[0] + 1, turns[0] + 2]);
    }

    #[test]
    fn a_worker_runs_the_explorers_tasks_and_logs_what_they_say() {
        let mut explorer = Explorer::new("exit 0");
        let run = explorer.shell("echo x >> @DIR@/ticks; echo ticked");
        explorer
            .registration
            .task
            .insert("tick".to_owned(), TaskRegistration { every: 3600, run });
        assert_eq!(
            explorer.until_idle(),
            ["task tick: ticked", "exiting: idle"]
        );
        // A worker runs each task when it starts, whenever it last ran:
        // what happened while no worker ran is noticed at once.
        explorer.until_idle();
        assert_eq!(explorer.lines("ticks").len(), 2);
    }

    #[test]
    fn workers_run_a_task_once_per_period_between_them() {
        let explorer = Explorer::new("exit 0");
        let tasks = BTreeMap::from([(
            "tick".to_owned(),
            TaskRegistration {
                every: 60,
                run: Vec::new(),
            },
        )]);
        let (one, two) = (explorer.conn(), explorer.conn());
        let mut first = Tasks::new(&one, &tasks);
        let mut second = Tasks::new(&two, &tasks);
        let due = |tasks: &mut Tasks<'_>| {
            // As if this worker's own period had passed.
            tasks.next.values_mut().for_each(|at| *at = Instant::now());
            tasks.due().expect("due").len()
        };
        // Each worker runs it when it starts.
        assert_eq!((due(&mut first), due(&mut second)), (1, 1));
        // Within the period, neither runs it again.
        assert_eq!((due(&mut first), due(&mut second)), (0, 0));
        // Once the period has passed, exactly one of them does.
        one.execute("UPDATE tasks SET ran_at = ran_at - 60", [])
            .expect("age");
        assert_eq!(due(&mut first) + due(&mut second), 1);
        assert_eq!((due(&mut first), due(&mut second)), (0, 0));
    }

    #[test]
    fn an_idle_consumer_runs_its_idle_command_after_the_cooldown() {
        let mut explorer = Explorer::new("exit 0");
        let run = explorer.shell(r#"echo "$EXPL_CONSUMER" >> @DIR@/idle"#);
        explorer.consumer().idle = Some(IdleCommand { after: 1, run });
        let interrupt = Interrupt::default();
        let started = Instant::now();
        let reason = thread::scope(|scope| {
            let worker =
                scope.spawn(|| explorer.run(ProcessOptions::default(), interrupt.clone(), None));
            explorer.wait_for("idle");
            let waited = started.elapsed();
            interrupt.raise();
            assert!(waited >= Duration::from_millis(900), "ran after {waited:?}");
            worker.join().expect("worker").0
        });
        assert_eq!(reason, "stopped");
        assert_eq!(explorer.lines("idle")[0], CONSUMER);
    }

    #[test]
    fn under_a_daemon_a_consumer_placed_anywhere_runs_on_a_member() {
        let mut explorer = Explorer::new(
            r#"echo "$EXPL_OUTCOME" >> @DIR@/calls
               printf '{"outcome":"acknowledge","note":"on a member"}' > "$EXPL_OUTCOME""#,
        );
        explorer.consumer().placement = Placement::Any;
        let event = explorer.publish(None);
        let shared = tempfile::tempdir().expect("cluster");
        let cluster = Cluster::open(shared.path()).expect("cluster");
        let daemon = Interrupt::default();
        let seen = thread::scope(|scope| {
            let serving = scope.spawn(|| {
                cluster.serve(
                    "local",
                    &daemon,
                    |_| Ok(()),
                    |_| Ok(()),
                    // The member's supervisor, in this process: run the job,
                    // then hand the daemon a child that is already done.
                    |job, peer| {
                        cluster.run_job(job, peer, |_, command| Ok(command))?;
                        Command::new("true").spawn()
                    },
                )
            });
            let deadline = Instant::now() + scaled(Duration::from_secs(20));
            let leader = loop {
                if let Some(leader) = cluster.leader().expect("leader") {
                    break leader;
                }
                assert!(Instant::now() < deadline, "no leader was elected");
                thread::sleep(Duration::from_millis(20));
            };
            let (authority, _term) = cluster.enter(&leader).expect("term");
            let options = ProcessOptions {
                once: false,
                exit_when_idle: true,
            };
            let (reason, seen) = explorer.run(options, Interrupt::default(), Some(&authority));
            assert_eq!(reason, "idle");
            daemon.raise();
            serving.join().expect("daemon").expect("serve");
            seen
        });
        assert_eq!(seen, ["reacted e1: on a member", "exiting: idle"]);
        assert_eq!(explorer.status(event), "done");
        // The outcome was reported where every member can reach it.
        let outcome = PathBuf::from(&explorer.lines("calls")[0]);
        assert!(
            outcome.starts_with(cluster.root()),
            "{} is not under the cluster's directory",
            outcome.display()
        );
    }
}
