//! The contract between a [worker](crate::worker::Worker) and the command
//! it executes for one event.
//!
//! The worker describes the delivery in the command's environment and names
//! a file for the command's [`Outcome`]. A command that writes none
//! acknowledges the event by exiting with success, and asks for it to be
//! delivered again by failing.
//!
//! | Variable | Meaning |
//! | --- | --- |
//! | `EXPL_CONSUMER` | the consumer reacting |
//! | `EXPL_EVENT_ID` | the event's id in the log |
//! | `EXPL_EVENT` | the event's kind |
//! | `EXPL_SUBJECT` | what the event is about, when it names a subject |
//! | `EXPL_EVENT_CREATED_AT` | when the event was recorded |
//! | `EXPL_ATTEMPTS` | how often the event has been delivered, this time included |
//! | `EXPL_MAX_ATTEMPTS` | the attempt after which it is given up |
//! | `EXPL_LEASE` | the lease token of this delivery |
//! | `EXPL_WORKER` | the id of the worker's process record |
//! | `EXPL_TURN` | a number that advances with each command the worker runs, for a command that takes turns among alternatives |
//! | `EXPL_REACTION` | `react`, or `abandon` to clean up after a holder that died on the last attempt |
//! | `EXPL_OUTCOME` | the file to write the outcome to |
//! | `TRACEPARENT`, `TRACESTATE` | the delivery's span, when it is traced |
//!
//! The command runs in the explorer's directory and finds the explorer's
//! database in [`DATABASE`](crate::daemon::DATABASE), as do the explorer's
//! tasks and idle commands.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Interrupt;
use crate::delivery::{Delivery, Event, Inbox, LeaseToken};
use crate::event::EventId;

/// The consumer reacting.
pub const CONSUMER: &str = "EXPL_CONSUMER";
/// The event's id in the log.
pub const EVENT_ID: &str = "EXPL_EVENT_ID";
/// The event's kind.
pub const EVENT: &str = "EXPL_EVENT";
/// What the event is about.
pub const SUBJECT: &str = "EXPL_SUBJECT";
/// When the event was recorded.
pub const CREATED_AT: &str = "EXPL_EVENT_CREATED_AT";
/// How often the event has been delivered, this time included.
pub const ATTEMPTS: &str = "EXPL_ATTEMPTS";
/// The attempt after which the event is given up.
pub const MAX_ATTEMPTS: &str = "EXPL_MAX_ATTEMPTS";
/// The lease token of the delivery.
pub const LEASE: &str = "EXPL_LEASE";
/// The id of the worker's process record.
pub const WORKER: &str = "EXPL_WORKER";
/// A number that advances with each command the worker runs.
pub const TURN: &str = "EXPL_TURN";
/// What the command is asked to do: see [`Mode`].
pub const REACTION: &str = "EXPL_REACTION";
/// The file the command writes its [`Outcome`] to.
pub const OUTCOME: &str = "EXPL_OUTCOME";
/// The delivery's span as W3C trace context.
pub const TRACEPARENT: &str = "TRACEPARENT";
/// The vendor state of the delivery's span.
pub const TRACESTATE: &str = "TRACESTATE";

/// Every variable of the contract. A command that starts programs of its
/// own which must not learn of the delivery (an agent CLI) keeps these out
/// of their environment.
pub const VARIABLES: [&str; 14] = [
    CONSUMER,
    EVENT_ID,
    EVENT,
    SUBJECT,
    CREATED_AT,
    ATTEMPTS,
    MAX_ATTEMPTS,
    LEASE,
    WORKER,
    TURN,
    REACTION,
    OUTCOME,
    TRACEPARENT,
    TRACESTATE,
];

/// How often the command looks whether it was told to stop.
const WATCH: Duration = Duration::from_millis(50);

/// What a command is asked to do with its delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// React to the event.
    React,
    /// The delivery ended for good without a reaction settling it: its
    /// holder died on the last attempt. Clean up what it left behind.
    Abandon,
}

impl Mode {
    /// The value of [`REACTION`].
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::React => "react",
            Self::Abandon => "abandon",
        }
    }
}

/// The environment does not describe a delivery.
#[derive(Debug, Error)]
pub enum ReactionError {
    /// A variable of the contract is not set.
    #[error("`{0}` is not set: this command is run by a worker, for one event")]
    Missing(&'static str),
    /// A variable holds a value the contract does not allow.
    #[error("`{name}` is `{value}`, which is not {expected}")]
    Invalid {
        /// The variable.
        name: &'static str,
        /// Its value.
        value: String,
        /// What it should have been.
        expected: &'static str,
    },
}

/// One delivery, as the command executed for it sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    /// The consumer reacting.
    pub consumer: String,
    /// The leased event.
    pub delivery: Delivery,
    /// The attempt after which the event is given up.
    pub max_attempts: i64,
    /// The id of the worker's process record.
    pub worker: i64,
    /// A number that advances with each command the worker runs, starting
    /// somewhere different for each worker. A command that takes turns
    /// among alternatives (the CLIs of an agent) starts from it: every
    /// command is a process of its own and would otherwise have no memory
    /// of whose turn it is.
    pub turn: u64,
    /// What the command is asked to do.
    pub mode: Mode,
    /// Where the outcome is reported, if anywhere.
    pub outcome: Option<PathBuf>,
}

impl Reaction {
    /// The delivery this process was started for.
    ///
    /// # Errors
    ///
    /// [`ReactionError`] when the environment does not describe one.
    pub fn from_env() -> Result<Self, ReactionError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ReactionError> {
        let required = |name: &'static str| get(name).ok_or(ReactionError::Missing(name));
        let number = |name: &'static str, value: String| {
            value.parse::<i64>().map_err(|_| ReactionError::Invalid {
                name,
                value,
                expected: "a number",
            })
        };
        let mode = match required(REACTION)?.as_str() {
            "react" => Mode::React,
            "abandon" => Mode::Abandon,
            other => {
                return Err(ReactionError::Invalid {
                    name: REACTION,
                    value: other.to_owned(),
                    expected: "`react` or `abandon`",
                });
            }
        };
        Ok(Self {
            consumer: required(CONSUMER)?,
            delivery: Delivery {
                event: Event {
                    id: EventId(number(EVENT_ID, required(EVENT_ID)?)?),
                    kind: required(EVENT)?,
                    subject: get(SUBJECT)
                        .filter(|subject| !subject.is_empty())
                        .map(|subject| number(SUBJECT, subject))
                        .transpose()?,
                    created_at: get(CREATED_AT).unwrap_or_default(),
                },
                attempts: number(ATTEMPTS, required(ATTEMPTS)?)?,
                token: LeaseToken::new(required(LEASE)?),
            },
            max_attempts: number(MAX_ATTEMPTS, required(MAX_ATTEMPTS)?)?,
            worker: number(WORKER, required(WORKER)?)?,
            turn: match get(TURN) {
                Some(turn) => turn.parse().map_err(|_| ReactionError::Invalid {
                    name: TURN,
                    value: turn,
                    expected: "a number",
                })?,
                None => u64::from(std::process::id()),
            },
            mode,
            outcome: get(OUTCOME)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        })
    }

    /// The variables that describe this reaction to its command.
    #[must_use]
    pub fn environment(&self) -> Vec<(&'static str, String)> {
        let event = &self.delivery.event;
        let mut variables = vec![
            (CONSUMER, self.consumer.clone()),
            (EVENT_ID, event.id.0.to_string()),
            (EVENT, event.kind.clone()),
            (CREATED_AT, event.created_at.clone()),
            (ATTEMPTS, self.delivery.attempts.to_string()),
            (MAX_ATTEMPTS, self.max_attempts.to_string()),
            (LEASE, self.delivery.token.as_str().to_owned()),
            (WORKER, self.worker.to_string()),
            (TURN, self.turn.to_string()),
            (REACTION, self.mode.as_str().to_owned()),
        ];
        if let Some(subject) = event.subject {
            variables.push((SUBJECT, subject.to_string()));
        }
        if let Some(outcome) = &self.outcome {
            variables.push((OUTCOME, outcome.display().to_string()));
        }
        variables
    }

    /// Whether no attempt is left after this one: a reaction that fails now
    /// must first clean up whatever it would otherwise leave to a retry.
    #[must_use]
    pub fn is_last_attempt(&self) -> bool {
        self.delivery.attempts >= self.max_attempts
    }

    /// Report `outcome` to the worker. Without a file to report to (the
    /// command was started by hand) nothing is written.
    ///
    /// # Errors
    ///
    /// I/O failure writing the outcome file.
    pub fn report(&self, outcome: &Outcome) -> io::Result<()> {
        match &self.outcome {
            Some(path) => outcome.write(path),
            None => Ok(()),
        }
    }

    /// The interrupt of this reaction, raised when the command is told to
    /// stop (SIGTERM or SIGINT) or its worker is gone. The worker stops a
    /// command for two reasons, and the command must tell them apart
    /// before it acts on either: the worker is stopping, so the work is
    /// given back; or the lease was lost, so the work now belongs to
    /// another holder and must be left as it is. The lease in `database` is
    /// looked at first, and a lost one raises the interrupt as
    /// [lost](Interrupt::lease_lost).
    ///
    /// # Errors
    ///
    /// I/O error when the signal handlers cannot be installed.
    pub fn interrupt(&self, database: &Path) -> io::Result<Interrupt> {
        let told = Arc::new(AtomicBool::new(false));
        for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
            signal_hook::flag::register(signal, Arc::clone(&told))?;
        }
        let process = Interrupt::default();
        let reaction = process.for_reaction();
        let watched = reaction.clone();
        let parent = std::os::unix::process::parent_id();
        let database = database.to_path_buf();
        let consumer = self.consumer.clone();
        let delivery = self.delivery.clone();
        thread::spawn(move || {
            while !told.load(Ordering::SeqCst) && std::os::unix::process::parent_id() == parent {
                thread::sleep(WATCH);
            }
            // Looked at, never renewed: a lease whose worker is gone must
            // run out, so that the event can be delivered again.
            let lost = Inbox::open(&database, &consumer)
                .is_ok_and(|inbox| matches!(inbox.holds(&delivery), Ok(false)));
            if lost {
                watched.lose_lease();
            } else {
                process.raise();
            }
        });
        Ok(reaction)
    }
}

/// What became of one reaction, as its command reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The event is handled.
    Acknowledge {
        /// One line for the log and the delivery record.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// The newest event this reaction took into account. A consumer
        /// that coalesces covers its earlier events up to here; without
        /// it, up to the newest event recorded before the reaction began.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        covered: Option<i64>,
        /// What kind of acknowledgement this is, for metrics; plain
        /// `acknowledged` when unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    /// Give the event back; this attempt does not count.
    Release {
        /// Why.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// The reaction failed: deliver the event again, or give it up when no
    /// attempt is left.
    Retry {
        /// What went wrong.
        note: String,
    },
    /// The lease was lost: the event belongs to another holder now.
    Lost,
}

impl Outcome {
    /// A plain acknowledgement with `note`.
    #[must_use]
    pub fn acknowledged(note: impl Into<String>) -> Self {
        Self::Acknowledge {
            note: Some(note.into()),
            covered: None,
            kind: None,
        }
    }

    /// Write the outcome to `path`, whole or not at all.
    ///
    /// # Errors
    ///
    /// I/O failure.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let directory = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut file, self)?;
        file.flush()?;
        file.persist(path)?;
        Ok(())
    }

    /// The outcome written to `path`; `None` when the command wrote none.
    ///
    /// # Errors
    ///
    /// I/O failure, or a file that does not hold an outcome.
    pub fn read(path: &Path) -> io::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Ok(None),
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reaction() -> Reaction {
        Reaction {
            consumer: "measurer".to_owned(),
            delivery: Delivery {
                event: Event {
                    id: EventId(7),
                    kind: "scenario.added".to_owned(),
                    subject: Some(3),
                    created_at: "2026-11-23T00:00:00.000Z".to_owned(),
                },
                attempts: 2,
                token: LeaseToken::new("token"),
            },
            max_attempts: 3,
            worker: 5,
            turn: 41,
            mode: Mode::React,
            outcome: Some(PathBuf::from("/tmp/outcome.json")),
        }
    }

    #[test]
    fn a_command_reads_back_the_delivery_its_worker_described() {
        let sent = reaction();
        let environment = sent.environment();
        let read = Reaction::from_lookup(|name| {
            environment
                .iter()
                .find(|(variable, _)| *variable == name)
                .map(|(_, value)| value.clone())
        })
        .expect("reaction");
        assert_eq!(read, sent);
        assert!(!read.is_last_attempt());
        for (variable, _) in &environment {
            assert!(VARIABLES.contains(variable), "{variable}");
        }

        // An event about nothing in particular names no subject.
        let mut plain = sent.clone();
        plain.delivery.event.subject = None;
        plain.delivery.attempts = 3;
        plain.mode = Mode::Abandon;
        let environment = plain.environment();
        let read = Reaction::from_lookup(|name| {
            environment
                .iter()
                .find(|(variable, _)| *variable == name)
                .map(|(_, value)| value.clone())
        })
        .expect("reaction");
        assert_eq!(read, plain);
        assert!(read.is_last_attempt());
    }

    #[test]
    fn an_environment_that_describes_no_delivery_is_refused() {
        let missing = Reaction::from_lookup(|_| None).expect_err("nothing set");
        assert!(
            matches!(missing, ReactionError::Missing(REACTION)),
            "{missing}"
        );
        let environment = reaction().environment();
        let invalid = Reaction::from_lookup(|name| {
            if name == ATTEMPTS {
                return Some("twice".to_owned());
            }
            environment
                .iter()
                .find(|(variable, _)| *variable == name)
                .map(|(_, value)| value.clone())
        })
        .expect_err("attempts");
        assert!(invalid.to_string().contains("EXPL_ATTEMPTS"), "{invalid}");
    }

    #[test]
    fn outcomes_are_written_whole_and_read_back() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("outcome.json");
        assert_eq!(Outcome::read(&path).expect("absent"), None);
        std::fs::write(&path, "\n").expect("empty");
        assert_eq!(Outcome::read(&path).expect("empty"), None);
        for outcome in [
            Outcome::Acknowledge {
                note: Some("plan 1".to_owned()),
                covered: Some(12),
                kind: None,
            },
            Outcome::acknowledged("done"),
            Outcome::Release { note: None },
            Outcome::Retry {
                note: "did not compile".to_owned(),
            },
            Outcome::Lost,
        ] {
            outcome.write(&path).expect("write");
            assert_eq!(Outcome::read(&path).expect("read"), Some(outcome));
        }
        std::fs::write(&path, r#"{"outcome":"retry","note":"by hand"}"#).expect("by hand");
        assert_eq!(
            Outcome::read(&path).expect("read"),
            Some(Outcome::Retry {
                note: "by hand".to_owned()
            })
        );
        std::fs::write(&path, "not json").expect("garbage");
        assert!(Outcome::read(&path).is_err());
    }
}
