//! Cluster coordination over a shared filesystem with cross-host file locks.
//! Each member uses a distinct name and the same absolute cluster path.
//! The lifetime of an exclusive election lock defines the leader's term.

mod job;

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::Interrupt;

pub use job::{CommandSpec, Outcome};

const POLL: Duration = Duration::from_millis(100);

/// Shared coordination and command transport for one application.
#[derive(Debug, Clone)]
pub struct Cluster {
    root: PathBuf,
}

/// A single daemon incarnation. Names can be reused after a daemon exits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    name: String,
    session: String,
}

impl Peer {
    /// The member's configured name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// An elected daemon's authority. Application subprocesses hold a guard for
/// their lifetime, so a successor waits for old subprocesses before recovery.
#[derive(Debug, Clone)]
pub struct Authority {
    cluster: Cluster,
    peer: Peer,
}

impl Authority {
    /// The cluster to which this authority belongs.
    #[must_use]
    pub fn cluster(&self) -> &Cluster {
        &self.cluster
    }

    /// The elected daemon's current session.
    #[must_use]
    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    /// Whether this term is still live. An unreadable lock fails closed.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.cluster.is_live(&self.peer).unwrap_or(false)
    }
}

impl Cluster {
    /// Open a shared cluster directory, creating its queues if needed.
    ///
    /// # Errors
    ///
    /// Filesystem or path errors.
    pub fn open(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        for subdir in ["members", "sessions", "requests", "staging", "explorers"] {
            fs::create_dir_all(root.join(subdir))?;
        }
        Ok(Self { root })
    }

    /// The canonical cluster directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Find a member currently accepting jobs.
    ///
    /// # Errors
    ///
    /// Filesystem errors or an invalid member name.
    pub fn member(&self, name: &str) -> io::Result<Option<Peer>> {
        check_name(name)?;
        let peer = read_optional::<Peer>(&self.root.join("members").join(name))?;
        self.live_peer(peer)
    }

    /// Find the current leader after promotion has completed.
    ///
    /// # Errors
    ///
    /// Filesystem errors.
    pub fn leader(&self) -> io::Result<Option<Peer>> {
        self.live_peer(read_optional(&self.root.join("leader"))?)
    }

    fn live_peer(&self, peer: Option<Peer>) -> io::Result<Option<Peer>> {
        match peer {
            Some(peer) if self.is_live(&peer)? => Ok(Some(peer)),
            _ => Ok(None),
        }
    }

    fn is_live(&self, peer: &Peer) -> io::Result<bool> {
        occupied(&self.root.join("sessions").join(&peer.session))
    }

    /// Enter a published term. Keep the returned file open while touching
    /// application state.
    ///
    /// # Errors
    ///
    /// Filesystem errors or an expired term.
    pub fn enter(&self, peer: &Peer) -> io::Result<(Authority, File)> {
        let guard = lock_file(&self.root.join("authority.lock"))?;
        // Do not wait behind a successor's recovery: the old request has
        // no right to start again once its session expires.
        try_shared(&guard)?;
        if self.leader()?.as_ref() != Some(peer) {
            return Err(io::Error::other("cluster leader changed"));
        }
        Ok((
            Authority {
                cluster: self.clone(),
                peer: peer.clone(),
            },
            guard,
        ))
    }

    /// Run a daemon until interrupted. A follower forwards control jobs
    /// and executes compute jobs. The current leader does both and also
    /// calls `promote` (once per term) and `tick` while elected.
    ///
    /// `supervisor` must create a fresh process calling [`Self::run_job`]
    /// with the provided job name and peer. Each supervisor survives the
    /// daemon long enough to cancel and reap its job if the daemon dies.
    ///
    /// # Errors
    ///
    /// Filesystem, application startup or supervisor errors.
    pub fn serve(
        &self,
        name: &str,
        interrupt: &Interrupt,
        mut promote: impl FnMut(&Authority) -> io::Result<()>,
        mut tick: impl FnMut(&Authority) -> io::Result<()>,
        mut supervisor: impl FnMut(&str, &Peer) -> io::Result<Child>,
    ) -> io::Result<()> {
        check_name(name)?;
        let member = self.root.join("members").join(name);
        let _member = try_lock(&member.with_extension("lock"))?.ok_or_else(|| {
            io::Error::other(format!("cluster member `{name}` is already running"))
        })?;
        let live = tempfile::Builder::new()
            .prefix("peer-")
            .tempfile_in(self.root.join("sessions"))?;
        live.as_file().lock()?;
        let peer = Peer {
            name: name.to_owned(),
            session: live
                .path()
                .file_name()
                .ok_or_else(|| io::Error::other("session file has no name"))?
                .to_string_lossy()
                .into_owned(),
        };
        atomic_json(&member, &peer)?;

        let authority = Authority {
            cluster: self.clone(),
            peer: peer.clone(),
        };
        let mut election = None;
        let mut children = Vec::new();
        while !interrupt.is_raised() {
            if election.is_none()
                && let Some(leader) = try_lock(&self.root.join("election.lock"))?
                && let Some(barrier) = try_lock(&self.root.join("authority.lock"))?
            {
                promote(&authority)?;
                atomic_json(&self.root.join("leader"), &peer)?;
                drop(barrier);
                election = Some(leader);
            }
            if election.is_some() {
                tick(&authority)?;
            }
            self.dispatch(&peer, election.is_some(), &mut children, &mut supervisor)?;
            thread::sleep(POLL);
        }
        Ok(())
    }
}

#[cfg(test)]
impl Cluster {
    /// An authority whose session is held by the returned file, without a
    /// serving daemon: its term ends when the file is dropped.
    pub(crate) fn test_term(&self, name: &str) -> (Authority, tempfile::NamedTempFile) {
        let session = tempfile::Builder::new()
            .prefix("peer-")
            .tempfile_in(self.root.join("sessions"))
            .expect("session");
        session.as_file().lock().expect("hold the session");
        let peer = Peer {
            name: name.to_owned(),
            session: session
                .path()
                .file_name()
                .expect("session name")
                .to_string_lossy()
                .into_owned(),
        };
        let authority = Authority {
            cluster: self.clone(),
            peer,
        };
        (authority, session)
    }
}

pub(crate) fn check_name(name: &str) -> io::Result<()> {
    if !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(io::Error::new(
            ErrorKind::InvalidInput,
            "member names use letters, digits, '-' and '_'",
        ))
    }
}

pub(crate) fn lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn try_lock(path: &Path) -> io::Result<Option<File>> {
    let file = lock_file(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

fn try_shared(file: &File) -> io::Result<()> {
    match file.try_lock_shared() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(io::Error::other("cluster leadership is changing")),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Probe a lock without creating it. A vanished session cannot come back.
fn occupied(path: &Path) -> io::Result<bool> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

pub(crate) fn read_optional<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn atomic_json(path: &Path, data: &impl Serialize) -> io::Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().expect("file parent"))?;
    serde_json::to_writer(&mut temp, data)?;
    temp.persist(path)?;
    Ok(())
}

fn file_command(command: &mut Command, directory: &Path) -> io::Result<()> {
    use std::process::Stdio;
    command
        .stdin(File::open(directory.join("stdin"))?)
        .stdout(Stdio::from(File::create(directory.join("stdout"))?))
        .stderr(Stdio::from(File::create(directory.join("stderr"))?));
    Ok(())
}
