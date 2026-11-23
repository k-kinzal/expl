//! Command configuration shared by orchestration applications.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::DriverKind;

/// A declared [`Timeout`] is not a usable duration.
#[derive(Debug, Error)]
pub enum TimeoutError {
    /// The value is zero or negative.
    #[error("`{0}` is not a positive duration")]
    NotPositive(Timeout),
    /// The text is not a duration.
    #[error("`{value}` is not a duration: {source}")]
    Unparsable {
        /// The declared text.
        value: String,
        /// Underlying error.
        #[source]
        source: humantime::DurationError,
    },
}

/// How long one CLI attempt may run before expl stops it: whole seconds
/// (`timeout = 1800`) or a humantime string (`timeout = "30m"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Timeout {
    /// Whole seconds.
    Seconds(i64),
    /// A humantime duration such as `"90s"` or `"1h 30m"`.
    Text(String),
}

impl Timeout {
    /// The declared duration.
    ///
    /// # Errors
    ///
    /// [`TimeoutError`] when the value is zero, negative, or not a duration.
    pub fn duration(&self) -> Result<Duration, TimeoutError> {
        let duration = match self {
            Self::Seconds(seconds) => u64::try_from(*seconds)
                .map(Duration::from_secs)
                .map_err(|_| TimeoutError::NotPositive(self.clone()))?,
            Self::Text(text) => humantime::parse_duration(text.trim()).map_err(|source| {
                TimeoutError::Unparsable {
                    value: text.clone(),
                    source,
                }
            })?,
        };
        if duration.is_zero() {
            return Err(TimeoutError::NotPositive(self.clone()));
        }
        Ok(duration)
    }
}

impl fmt::Display for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Seconds(seconds) => write!(f, "{seconds}"),
            Self::Text(text) => f.write_str(text),
        }
    }
}

/// One alternative CLI of an agent: the same fields that declare the
/// agent's primary CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliDeclaration {
    /// Which driver translates for this CLI.
    pub driver: DriverKind,
    /// Program and leading arguments, e.g. `["codex"]`.
    pub command: Vec<String>,
    /// Extra arguments appended after the driver's own flags.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for this CLI, over the agent's `env`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// How long one attempt of this CLI may run, over the agent's `timeout`.
    #[serde(default)]
    pub timeout: Option<Timeout>,
}

/// Run a local utility until it exits or is interrupted and reap its process group.
/// Child output is collected from files so inherited pipes cannot stall it.
///
/// # Errors
///
/// I/O failure, interruption, or a process group that could not be reaped.
pub fn run_supervised(
    mut command: std::process::Command,
    interrupt: &crate::Interrupt,
) -> std::io::Result<std::process::Output> {
    use std::io::{self, Read, Seek, SeekFrom};
    use std::os::unix::process::CommandExt;
    use std::process::{Output, Stdio};

    use crate::agent::supervisor::{End, Reaped, reap_group, wait};

    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let end = wait(&mut child, || interrupt.is_raised(), None);
    if reap_group(child.id()) == Reaped::Lingering {
        return Err(io::Error::other("command left a live process group"));
    }
    let End::Exited(status) = end? else {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "command interrupted",
        ));
    };
    let read = |file: &mut std::fs::File| -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    Ok(Output {
        status,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
    })
}
