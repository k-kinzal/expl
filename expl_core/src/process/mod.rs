//! Workers and the process table behind `expl ps`.
//!
//! The delivery loop is the runtime's: a
//! [`Worker`](expl_runtime::worker::Worker) leases an event from a
//! consumer's inbox and executes the command registered for it, which for
//! this explorer is one [reaction](crate::react) of the consumer's handler.
//! Every worker has a [`ProcessRecord`] in the project database — the
//! operator's durable memory of what runs and what ran, with its log file,
//! heartbeat, current activity, and exit reason.

pub use expl_runtime::Interrupt;
pub use expl_runtime::process::{
    HEARTBEAT_SECONDS, ProcessId, ProcessOptions, ProcessRecord, ProcessState, ProcessStatus,
    Processes, STALE_SECONDS, STARTUP_SECONDS,
};
pub use expl_runtime::worker::MAX_ATTEMPTS;

use crate::project::Project;
use crate::store::{self, StoreError};

/// The process table of `project`, its database brought up to date first.
///
/// # Errors
///
/// [`StoreError`] when the database cannot be opened or migrated.
pub fn processes(project: &Project) -> Result<Processes, StoreError> {
    Ok(Processes::over(
        store::open(&project.database_path())?,
        &project.processes_dir(),
    ))
}
