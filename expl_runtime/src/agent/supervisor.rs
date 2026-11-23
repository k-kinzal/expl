//! Shared process-group supervision for local and member execution.

use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long an interrupted or timed-out invocation gets to exit after
/// SIGTERM.
pub(crate) const TERMINATION_GRACE: Duration = Duration::from_secs(5);

/// How long children an exited CLI left behind get to exit after SIGTERM
/// before they are killed.
pub(crate) const LEFTOVER_GRACE: Duration = Duration::from_secs(2);

/// How long a process group gets to vanish after SIGKILL. A member that
/// outlives it is stuck where it cannot even be killed; past this bound the
/// attempt fails with `AgentError::Lingering` instead of handing the
/// working directory on.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// How waiting for a child ended.
#[derive(Debug, Clone, Copy)]
pub(crate) enum End {
    /// It exited by itself.
    Exited(ExitStatus),
    /// The hosting process was asked to stop, so the child was stopped.
    Interrupted,
    /// The deadline passed, so the child was stopped.
    TimedOut,
}

/// Wait for the child until it exits; on interrupt, or once `deadline`
/// passes, stop its whole process group (TERM, then KILL after a grace
/// period) until the leader is gone. Members that outlive the leader are
/// [`reap_group`]'s to stop.
pub(crate) fn wait(
    child: &mut Child,
    interrupted: impl Fn() -> bool,
    deadline: Option<Instant>,
) -> std::io::Result<End> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(End::Exited(status));
        }
        let end = if interrupted() {
            End::Interrupted
        } else if deadline.is_some_and(|at| Instant::now() >= at) {
            End::TimedOut
        } else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };
        signal_group(child.id(), "-TERM");
        let grace = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < grace {
            if child.try_wait()?.is_some() {
                return Ok(end);
            }
            thread::sleep(Duration::from_millis(100));
        }
        signal_group(child.id(), "-KILL");
        child.wait()?;
        return Ok(end);
    }
}

/// Send `signal` to the process group led by `pid`; `true` when the group
/// still had a member to receive it.
pub(crate) fn signal_group(pid: u32, signal: &str) -> bool {
    // Best effort: the group may already be gone.
    Command::new("kill")
        .arg(signal)
        .arg("--")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// What [`reap_group`] found of a process group whose leader is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reaped {
    /// Nothing was left of it.
    Empty,
    /// Members were left; they are gone now.
    Stopped,
    /// Members were left, and some survived SIGKILL for [`KILL_GRACE`].
    Lingering,
}

/// Stop whatever is left of the process group led by the exited CLI `pid`
/// (TERM, then KILL after a grace period), and wait until no member is left,
/// so background children can neither hold its pipes open nor keep working
/// in its directory once the attempt is over.
pub(crate) fn reap_group(pid: u32) -> Reaped {
    if !signal_group(pid, "-TERM") {
        return Reaped::Empty;
    }
    if vanishes(pid, LEFTOVER_GRACE) {
        return Reaped::Stopped;
    }
    signal_group(pid, "-KILL");
    if vanishes(pid, KILL_GRACE) {
        Reaped::Stopped
    } else {
        Reaped::Lingering
    }
}

/// Whether the process group led by `pid` has no member left by `grace`
/// from now. A killed member stays a member until its parent reaps it,
/// so this also waits for that.
fn vanishes(pid: u32, grace: Duration) -> bool {
    let by = Instant::now() + grace;
    loop {
        if !signal_group(pid, "-0") {
            return true;
        }
        if Instant::now() >= by {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}
