//! Requests are published by rename. The requester holds `owner`; the job
//! supervisor holds `active` until its process group has been collected.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Cluster, POLL, Peer, atomic_json, file_command, occupied, read_optional, try_lock};
use crate::Interrupt;
use crate::agent::supervisor::{End, Reaped, reap_group, wait};

/// Command information portable between members with the same mounted paths.
#[derive(Debug, Serialize, Deserialize)]
pub struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, Option<OsString>)>,
    directory: PathBuf,
}

impl CommandSpec {
    /// Capture a command's explicit arguments, environment and directory.
    /// The executable is resolved using the target member's environment.
    #[must_use]
    pub fn capture(command: &Command, directory: &Path) -> Self {
        Self {
            program: command.get_program().to_owned(),
            args: command.get_args().map(ToOwned::to_owned).collect(),
            env: command
                .get_envs()
                .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
                .collect(),
            directory: command.get_current_dir().unwrap_or(directory).to_owned(),
        }
    }

    fn command(self) -> Command {
        let mut command = Command::new(self.program);
        command
            .args(self.args)
            .current_dir(self.directory)
            .process_group(0);
        for (key, value) in self.env {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command
    }
}

/// A supervised cluster command's outcome.
#[derive(Debug, Serialize, Deserialize)]
pub enum Outcome {
    /// Raw Unix wait status, including signal exits.
    Exited(i32),
    /// Requester or daemon stopped, or the requester cancelled.
    Interrupted,
    /// The command exceeded its attempt timeout.
    TimedOut,
    /// A process group survived termination and still owns its workspace.
    Lingering {
        /// The group id on the executing member.
        group: u32,
    },
    /// The command could not be launched or supervised.
    Failed(String),
}

#[derive(Debug, Serialize, Deserialize)]
enum Target {
    Control(Peer),
    Compute(Peer),
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    target: Target,
    command: CommandSpec,
    timeout: Option<Duration>,
}

impl Cluster {
    /// Execute an application operation through `via`. Its daemon forwards
    /// the operation to the elected leader. Output is streamed to the caller.
    ///
    /// # Errors
    ///
    /// Transport or output errors. An operation that might have started is
    /// never automatically replayed after a leader loss.
    pub fn control(
        &self,
        via: &Peer,
        command: CommandSpec,
        interrupt: &Interrupt,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> io::Result<Outcome> {
        let request = Request {
            target: Target::Control(via.clone()),
            command,
            timeout: None,
        };
        let job = self.submit(&request, &[])?;
        self.receive(&job, &request.target, interrupt, stdout, stderr)
    }

    /// Execute a compute command on the least-loaded live member, including
    /// the leader. Members need only the command and a private workspace.
    ///
    /// # Errors
    ///
    /// Transport or output errors. Failed workers do not silently replay
    /// commands whose execution status is unknown.
    pub fn execute(
        &self,
        command: CommandSpec,
        input: Option<&str>,
        timeout: Option<Duration>,
        interrupt: &Interrupt,
        output: (&mut impl Write, &mut impl Write),
    ) -> io::Result<Outcome> {
        let schedule = super::lock_file(&self.root.join("schedule.lock"))?;
        schedule.lock()?;
        let peer = self.choose_member()?;
        let request = Request {
            target: Target::Compute(peer),
            command,
            timeout,
        };
        let job = self.submit(&request, input.unwrap_or("").as_bytes())?;
        drop(schedule);
        self.receive(&job, &request.target, interrupt, output.0, output.1)
    }

    fn choose_member(&self) -> io::Result<Peer> {
        let mut loads = BTreeMap::new();
        for entry in fs::read_dir(self.root.join("members"))? {
            let entry = entry?;
            if super::check_name(&entry.file_name().to_string_lossy()).is_err() {
                continue;
            }
            if let Some(peer) = self.live_peer(read_optional::<Peer>(&entry.path())?)? {
                loads.insert(peer.name.clone(), (0, peer));
            }
        }
        for job in fs::read_dir(self.root.join("requests"))? {
            let path = job?.path();
            if !path.join("result").exists()
                && let Some(Request {
                    target: Target::Compute(peer),
                    ..
                }) = read_optional(&path.join("request"))?
                && let Some((load, current)) = loads.get_mut(&peer.name)
                && *current == peer
            {
                *load += 1;
            }
        }
        loads
            .into_values()
            .min_by_key(|(load, _)| *load)
            .map(|(_, peer)| peer)
            .ok_or_else(|| io::Error::other("no live cluster members"))
    }

    fn submit(&self, request: &Request, input: &[u8]) -> io::Result<Pending> {
        let stage = tempfile::Builder::new()
            .prefix("job-")
            .tempdir_in(self.root.join("staging"))?;
        let owner = File::create(stage.path().join("owner"))?;
        owner.lock()?;
        fs::write(stage.path().join("stdin"), input)?;
        atomic_json(&stage.path().join("request"), request)?;
        let path = self
            .root
            .join("requests")
            .join(stage.path().file_name().expect("job name"));
        fs::rename(stage.path(), &path)?;
        Ok(Pending {
            path,
            _owner: owner,
        })
    }

    fn receive(
        &self,
        job: &Pending,
        target: &Target,
        interrupt: &Interrupt,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> io::Result<Outcome> {
        let path = &job.path;
        let mut out = Tail::new(path.join("stdout"));
        let mut err = Tail::new(path.join("stderr"));
        let mut cancelled = false;
        let mut output_error = None;
        loop {
            if output_error.is_none() {
                output_error = out.copy(stdout).and_then(|()| err.copy(stderr)).err();
            }
            if let Some(outcome) = read_optional(&path.join("result"))? {
                if let Some(error) = output_error {
                    return Err(error);
                }
                out.drain(stdout)?;
                err.drain(stderr)?;
                return Ok(outcome);
            }
            let started = read_optional::<Peer>(&path.join("started"))?;
            let usable = if let Some(peer) = &started {
                self.is_live(peer)? && occupied(&path.join("active"))?
            } else {
                match target {
                    Target::Compute(peer) => self.is_live(peer)?,
                    Target::Control(peer) => {
                        path.join("forwarded").exists() || self.is_live(peer)?
                    }
                }
            };
            if !cancelled && (!usable || interrupt.is_raised() || output_error.is_some()) {
                fs::write(path.join("cancel"), [])?;
                cancelled = true;
            }
            if cancelled && let Some(_active) = try_lock(&path.join("active"))? {
                if path.join("result").exists() {
                    continue;
                }
                if let Some(error) = output_error {
                    return Err(error);
                }
                if interrupt.is_raised() {
                    return Ok(Outcome::Interrupted);
                }
                return Err(io::Error::other(
                    "cluster member stopped before returning a result",
                ));
            }
            std::thread::sleep(POLL);
        }
    }

    pub(super) fn dispatch(
        &self,
        peer: &Peer,
        leader: bool,
        children: &mut Vec<(String, Child)>,
        supervisor: &mut impl FnMut(&str, &Peer) -> io::Result<Child>,
    ) -> io::Result<()> {
        let mut index = 0;
        while index < children.len() {
            if children[index].1.try_wait()?.is_some() {
                children.swap_remove(index).1.wait()?;
            } else {
                index += 1;
            }
        }
        for item in fs::read_dir(self.root.join("requests"))? {
            let item = item?;
            let job = item.path();
            if !occupied(&job.join("owner"))? {
                // An orphan that is already executing is left to its
                // supervisor, which will stop it before dropping its lock.
                if !occupied(&job.join("active"))? {
                    drop(fs::remove_dir_all(&job));
                }
                continue;
            }
            let Some(request) = read_optional::<Request>(&job.join("request"))? else {
                continue;
            };
            let eligible = match request.target {
                Target::Control(via) => {
                    if via == *peer
                        && !job.join("forwarded").exists()
                        && let Err(error) = fs::write(job.join("forwarded"), [])
                        && error.kind() != ErrorKind::NotFound
                    {
                        return Err(error);
                    }
                    leader && (via == *peer || job.join("forwarded").exists())
                }
                Target::Compute(target) => target == *peer,
            };
            let name = item.file_name().to_string_lossy().into_owned();
            if eligible
                && !job.join("cancel").exists()
                && !job.join("result").exists()
                && !job.join("started").exists()
                && !children.iter().any(|(active, _)| active == &name)
            {
                children.push((name.clone(), supervisor(&name, peer)?));
            }
        }
        Ok(())
    }

    /// Supervise one queued command in a dedicated process. `prepare` can
    /// supply application context (such as a guarded leader term) to control
    /// commands, and is called with `false` for compute commands.
    ///
    /// # Errors
    ///
    /// Filesystem errors, including an invalid job name.
    pub fn run_job(
        &self,
        name: &str,
        peer: &Peer,
        prepare: impl FnOnce(bool, Command) -> io::Result<Command>,
    ) -> io::Result<()> {
        super::check_name(name)?;
        let path = self.root.join("requests").join(name);
        if !path.is_dir() {
            return Ok(());
        }
        let _active = match try_lock(&path.join("active")) {
            Ok(Some(lock)) => lock,
            Ok(None) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if path.join("started").exists()
            || path.join("cancel").exists()
            || !occupied(&path.join("owner"))?
            || !self.is_live(peer)?
        {
            return Ok(());
        }
        let Some(request) = read_optional::<Request>(&path.join("request"))? else {
            return Ok(());
        };
        let control = matches!(request.target, Target::Control(_));
        let _authority = if control {
            match self.enter(peer) {
                Ok((_, guard)) => Some(guard),
                Err(_) => return Ok(()),
            }
        } else {
            None
        };
        atomic_json(&path.join("started"), peer)?;
        let result = self
            .supervise(&path, peer, request, prepare)
            .unwrap_or_else(|error| Outcome::Failed(error.to_string()));
        atomic_json(&path.join("result"), &result)
    }

    fn supervise(
        &self,
        path: &Path,
        peer: &Peer,
        request: Request,
        prepare: impl FnOnce(bool, Command) -> io::Result<Command>,
    ) -> io::Result<Outcome> {
        let control = matches!(request.target, Target::Control(_));
        let mut command = prepare(control, request.command.command())?;
        file_command(&mut command, path)?;
        let mut child = command.spawn()?;
        let pid = child.id();
        let deadline = request.timeout.map(|timeout| Instant::now() + timeout);
        let end = wait(
            &mut child,
            || {
                path.join("cancel").exists()
                    || !occupied(&path.join("owner")).unwrap_or(false)
                    || !self.is_live(peer).unwrap_or(false)
            },
            deadline,
        );
        if reap_group(pid) == Reaped::Lingering {
            return Ok(Outcome::Lingering { group: pid });
        }
        Ok(match end? {
            End::Exited(status) => Outcome::Exited(status.into_raw()),
            End::Interrupted => Outcome::Interrupted,
            End::TimedOut => Outcome::TimedOut,
        })
    }
}

struct Pending {
    path: PathBuf,
    _owner: File,
}

impl Drop for Pending {
    fn drop(&mut self) {
        // Leave a running command's spool available to its supervisor.
        if !occupied(&self.path.join("active")).unwrap_or(true) {
            drop(fs::remove_dir_all(&self.path));
        }
    }
}

struct Tail {
    path: PathBuf,
    file: Option<File>,
}

impl Tail {
    fn new(path: PathBuf) -> Self {
        Self { path, file: None }
    }

    fn drain(&mut self, output: &mut impl Write) -> io::Result<()> {
        while self.copy_chunk(output)? == 1024 * 1024 {}
        Ok(())
    }

    fn copy(&mut self, output: &mut impl Write) -> io::Result<()> {
        self.copy_chunk(output).map(drop)
    }

    fn copy_chunk(&mut self, output: &mut impl Write) -> io::Result<u64> {
        if self.file.is_none() {
            match File::open(&self.path) {
                Ok(file) => self.file = Some(file),
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
                Err(error) => return Err(error),
            }
        }
        if let Some(file) = &mut self.file {
            let copied = io::copy(&mut file.take(1024 * 1024), output)?;
            output.flush()?;
            return Ok(copied);
        }
        Ok(0)
    }
}
