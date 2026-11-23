//! The process table: the operator's durable memory of which workers run
//! and which ran.
//!
//! Every worker has a [`ProcessRecord`] in its explorer's database, with
//! its log file, heartbeat, current activity, and exit reason. Liveness is
//! the heartbeat, never the pid alone: a record that says `running` but
//! stopped beating is reported as dead, and the leases it held expire so
//! its events are delivered again.

mod processes;
mod record;

pub use processes::{HEARTBEAT_SECONDS, Processes, STALE_SECONDS, STARTUP_SECONDS};
pub use record::{ProcessId, ProcessRecord, ProcessState, ProcessStatus};

/// How long a worker lives.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessOptions {
    /// Exit after reacting to one event.
    pub once: bool,
    /// Exit as soon as the inbox has nothing deliverable.
    pub exit_when_idle: bool,
}
