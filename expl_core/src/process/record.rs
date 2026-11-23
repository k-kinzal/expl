//! [`ProcessRecord`] and its vocabulary.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Identity of a process record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessId(pub i64);

impl fmt::Display for ProcessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The stored lifecycle of a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    /// Registered for a detached start; the OS process has not adopted it yet.
    Created,
    /// The process said it is running (see [`ProcessState`] for liveness).
    Running,
    /// The process finished.
    Exited,
}

impl ProcessStatus {
    /// Stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Exited => "exited",
        }
    }
}

impl FromStr for ProcessStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "created" => Ok(Self::Created),
            "running" => Ok(Self::Running),
            "exited" => Ok(Self::Exited),
            other => Err(format!("unknown process status `{other}`")),
        }
    }
}

/// What the operator sees: the stored status reconciled with the heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    /// Created, not yet started.
    Created,
    /// Running and beating.
    Running,
    /// Says running, but its heartbeat stopped (crashed or killed); or was
    /// created but not adopted by its startup deadline (the launch was
    /// interrupted).
    Dead,
    /// Finished.
    Exited,
}

impl fmt::Display for ProcessState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Dead => "dead",
            Self::Exited => "exited",
        })
    }
}

/// The durable record of one process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRecord {
    /// Identity.
    pub id: ProcessId,
    /// Unique name (`<consumer>-<id>` unless given).
    pub name: String,
    /// The consumer whose events this worker processes.
    pub consumer: String,
    /// OS process id, once started.
    pub pid: Option<u32>,
    /// Reconciled state.
    pub state: ProcessState,
    /// Started in the background.
    pub detached: bool,
    /// Where its output goes.
    pub log_path: String,
    /// A stop was requested.
    pub stop_requested: bool,
    /// What it is doing right now.
    pub activity: Option<String>,
    /// Why it exited.
    pub exit_reason: Option<String>,
    /// Creation time.
    pub created_at: String,
    /// Startup deadline: a created record not adopted by then is
    /// abandoned and can no longer be adopted.
    pub start_by: Option<String>,
    /// Exit time.
    pub finished_at: Option<String>,
}

impl ProcessRecord {
    /// Whether the process is (believed to be) alive.
    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(self.state, ProcessState::Running | ProcessState::Created)
    }
}
