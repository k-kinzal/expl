//! Workers: the delivery loop of a consumer, and the lease it shares with
//! its heartbeat.
//!
//! A [`Worker`] holds no application code. It leases one event at a time
//! from its consumer's [inbox](crate::delivery::Inbox), executes the
//! command the explorer [registered](crate::registration) for that
//! consumer, and settles the delivery from the command's
//! [outcome](crate::reaction::Outcome). Replicas share the inbox, so
//! independent events run in parallel.

mod heartbeat;
mod log;
mod serve;
mod telemetry;

pub use heartbeat::{Active, with_heartbeat};
pub use log::ActivityLog;
pub use serve::{Activity, MAX_ATTEMPTS, Observer, REACTION_GRACE, Worker, WorkerError};
