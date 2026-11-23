//! Processes: running agents, managed the way Docker manages containers.
//!
//! A [`Process`] is one running instance of a declared
//! [`Agent`](crate::agent::Agent) — what a container is to an image. It
//! leases events from its agent's [`Inbox`](crate::event::Inbox), reacts to
//! each with the agent's role, and acknowledges it. Starting more processes
//! of the same agent is how exploration widens: they compete for the same
//! inbox, so independent events are handled in parallel.
//!
//! Every process has a [`ProcessRecord`] in the project database — the
//! operator's durable memory of what runs and what ran (`expl ps`), with
//! its log file (`expl logs`), heartbeat, current activity, and exit
//! reason. Liveness is the heartbeat, never the pid alone: a record that
//! says `running` but stopped beating is reported as dead, and the leases
//! it held expire so its events are delivered again.

mod capacity;
mod interrupt;
// Defining module named for the concept it defines — the path echo is deliberate.
mod process;
mod processes;
mod record;

pub use interrupt::Interrupt;
pub use process::{
    MAX_ATTEMPTS, Process, ProcessActivity, ProcessError, ProcessObserver, ProcessOptions,
};
pub use processes::{HEARTBEAT_SECONDS, Processes, STALE_SECONDS, STARTUP_SECONDS};
pub use record::{ProcessId, ProcessRecord, ProcessState, ProcessStatus};
