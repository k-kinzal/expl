//! Agent control: what the operator of a daemon allows the agents of every
//! explorer it hosts.
//!
//! The file `daemon.toml` in the daemon's directory holds it:
//!
//! ```toml
//! [agents]
//! concurrency = 2          # agent invocations at once, across every explorer
//!
//! [pool.specialized]
//! agents = 1               # at once, among the explorers of this pool
//!
//! [agent.claude]           # an agent explorers may name instead of declaring
//! driver = "claude_code"
//! command = ["claude"]
//! args = ["--model", "claude-opus-5-5"]
//! ```
//!
//! An explorer counts against the pool it registers (one of its own, named
//! after it, when it registers none). Limits are slots, as reaction
//! [capacity](crate::capacity) is: an invocation runs only while its
//! process holds a slot of the daemon's limit and one of its pool's. The
//! file is read again for each invocation, so the operator changes it
//! while explorers run.

use std::collections::BTreeMap;
use std::fs::{File, TryLockError};
use std::io::{self, ErrorKind};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Interrupt;
use crate::agent::DriverKind;
use crate::capacity::{self, Slot};
use crate::cluster::Cluster;
use crate::command::{CliDeclaration, Timeout, TimeoutError};

/// The file's name in the daemon's directory.
pub const FILE: &str = "daemon.toml";

/// How long a waiting invocation sleeps before it looks for a slot again.
const POLL: Duration = Duration::from_millis(200);

/// `daemon.toml` could not be used.
#[derive(Debug, Error)]
pub enum ControlError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is not valid TOML for agent control.
    #[error("invalid {path}: {source}")]
    Parse {
        /// The file.
        path: String,
        /// Underlying error.
        #[source]
        source: toml::de::Error,
    },
    /// A named agent declares no program to run.
    #[error("agent `{0}`: `command` is empty")]
    EmptyCommand(String),
    /// A named agent's `timeout` is zero or not a duration.
    #[error("agent `{agent}`: invalid `timeout`: {source}")]
    Timeout {
        /// The agent.
        agent: String,
        /// Why the value is unusable.
        #[source]
        source: TimeoutError,
    },
}

/// Waiting for an agent slot ended without one.
#[derive(Debug, Error)]
pub enum AcquireError {
    /// The process was asked to stop while it waited.
    #[error("interrupted while waiting for an agent slot")]
    Interrupted,
    /// A slot file could not be opened or locked.
    #[error("cannot use agent slots: {0}")]
    Io(#[from] io::Error),
}

/// The whole `daemon.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    /// Limits on every explorer's agents together.
    #[serde(default)]
    pub agents: Limits,
    /// Limits on the agents of each pool's explorers.
    #[serde(default)]
    pub pool: BTreeMap<String, PoolLimits>,
    /// Agents an explorer may name instead of declaring a CLI.
    #[serde(default)]
    pub agent: BTreeMap<String, NamedAgent>,
}

/// `[agents]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Agent invocations at once, across every explorer. None: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<NonZeroUsize>,
}

/// One `[pool.<name>]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolLimits {
    /// Agent invocations at once among the pool's explorers. None: no
    /// limit of the pool's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<NonZeroUsize>,
}

/// One `[agent.<name>]`: a CLI, and the alternatives it takes turns with.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedAgent {
    /// Which driver translates for its CLI.
    pub driver: DriverKind,
    /// Program and leading arguments.
    pub command: Vec<String>,
    /// Extra arguments appended after the driver's own flags.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the CLI.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// How long one attempt may run; also for alternatives without their
    /// own.
    #[serde(default)]
    pub timeout: Option<Timeout>,
    /// Further CLIs (`[[agent.<name>.alternative]]`).
    #[serde(default)]
    pub alternative: Vec<CliDeclaration>,
}

impl NamedAgent {
    /// Every CLI of the agent, primary first, each with the `timeout` that
    /// bounds it (its own, else the agent's).
    #[must_use]
    pub fn clis(&self) -> Vec<CliDeclaration> {
        let primary = CliDeclaration {
            driver: self.driver,
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            timeout: self.timeout.clone(),
        };
        let alternatives = self.alternative.iter().map(|cli| CliDeclaration {
            timeout: cli.timeout.clone().or_else(|| self.timeout.clone()),
            ..cli.clone()
        });
        std::iter::once(primary).chain(alternatives).collect()
    }
}

/// How many slots of a limit are taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// The limit.
    pub limit: usize,
    /// Slots held right now.
    pub in_use: usize,
}

/// The slots one agent invocation holds. Dropping them frees them.
#[derive(Debug)]
pub struct AgentSlots {
    _held: Vec<Slot>,
}

impl Control {
    /// Where the control file of `cluster` is.
    #[must_use]
    pub fn path(cluster: &Cluster) -> PathBuf {
        cluster.root().join(FILE)
    }

    /// The control in force for `cluster`: its `daemon.toml`, or no limits
    /// and no named agents when there is none.
    ///
    /// # Errors
    ///
    /// [`ControlError`] when the file is unreadable or invalid.
    pub fn load(cluster: &Cluster) -> Result<Self, ControlError> {
        let path = Self::path(cluster);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ControlError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        Self::parse_at(&text, &path.display().to_string())
    }

    /// Parse and validate control text.
    ///
    /// # Errors
    ///
    /// [`ControlError`] when it is not valid agent control.
    pub fn parse(text: &str) -> Result<Self, ControlError> {
        Self::parse_at(text, FILE)
    }

    fn parse_at(text: &str, path: &str) -> Result<Self, ControlError> {
        let control: Self = toml::from_str(text).map_err(|source| ControlError::Parse {
            path: path.to_owned(),
            source,
        })?;
        for (name, agent) in &control.agent {
            for cli in agent.clis() {
                if cli.command.first().is_none_or(|p| p.trim().is_empty()) {
                    return Err(ControlError::EmptyCommand(name.clone()));
                }
                if let Some(timeout) = &cli.timeout {
                    timeout.duration().map_err(|source| ControlError::Timeout {
                        agent: name.clone(),
                        source,
                    })?;
                }
            }
        }
        Ok(control)
    }

    /// The limit of `pool`'s own, if it has one.
    #[must_use]
    pub fn pool_limit(&self, pool: &str) -> Option<NonZeroUsize> {
        self.pool.get(pool).and_then(|limits| limits.agents)
    }

    /// Take a slot of every limit that applies to an explorer of `pool`,
    /// without waiting; `None` when one of them is full.
    ///
    /// # Errors
    ///
    /// I/O failure opening or locking a slot file.
    pub fn try_acquire(
        &self,
        cluster: &Cluster,
        pool: Option<&str>,
    ) -> io::Result<Option<AgentSlots>> {
        let mut held = Vec::new();
        let limits = [
            self.agents
                .concurrency
                .map(|limit| (everyone(cluster), limit)),
            pool.and_then(|pool| {
                self.pool_limit(pool)
                    .map(|limit| (pool_slots(cluster, pool), limit))
            }),
        ];
        for (directory, limit) in limits.into_iter().flatten() {
            std::fs::create_dir_all(&directory)?;
            let Some(slot) = capacity::try_slot(&directory, limit.get())? else {
                return Ok(None);
            };
            held.push(slot);
        }
        Ok(Some(AgentSlots { _held: held }))
    }

    /// The daemon-wide limit's use, when there is such a limit.
    ///
    /// # Errors
    ///
    /// I/O failure probing a slot file.
    pub fn usage(&self, cluster: &Cluster) -> io::Result<Option<Usage>> {
        self.agents
            .concurrency
            .map(|limit| in_use(&everyone(cluster), limit.get()))
            .transpose()
    }

    /// Each limited pool's use.
    ///
    /// # Errors
    ///
    /// I/O failure probing a slot file.
    pub fn pool_usage(&self, cluster: &Cluster) -> io::Result<BTreeMap<String, Usage>> {
        let mut usage = BTreeMap::new();
        for (name, limits) in &self.pool {
            if let Some(limit) = limits.agents {
                usage.insert(
                    name.clone(),
                    in_use(&pool_slots(cluster, name), limit.get())?,
                );
            }
        }
        Ok(usage)
    }
}

/// Wait until an agent invocation of an explorer of `pool` may run under
/// `cluster`'s control, and take its slots. The control file is read again
/// each time it looks, so a changed limit applies to whoever waits. A
/// control file that cannot be read is waited out as well: while its
/// operator is putting it right no agent starts, and no invocation fails
/// for it. `waiting` is told what is waited for, once each time that
/// changes.
///
/// # Errors
///
/// [`AcquireError::Interrupted`] when `interrupt` is raised first;
/// otherwise a slot file could not be used.
pub fn acquire(
    cluster: &Cluster,
    pool: Option<&str>,
    interrupt: &Interrupt,
    mut waiting: impl FnMut(&str),
) -> Result<AgentSlots, AcquireError> {
    let mut said = String::new();
    loop {
        if interrupt.is_raised() {
            return Err(AcquireError::Interrupted);
        }
        let reason = match Control::load(cluster) {
            Ok(control) => match control.try_acquire(cluster, pool)? {
                Some(slots) => return Ok(slots),
                None => "waiting for an agent slot".to_owned(),
            },
            Err(error) => format!("waiting for agent control: {error}"),
        };
        if reason != said {
            waiting(&reason);
            said = reason;
        }
        std::thread::sleep(POLL);
    }
}

fn everyone(cluster: &Cluster) -> PathBuf {
    cluster.root().join("slots").join("agents")
}

fn pool_slots(cluster: &Cluster, pool: &str) -> PathBuf {
    capacity::directory(&cluster.root().join("slots"), "pool", pool)
}

/// How many of the first `limit` slots in `directory` are held.
fn in_use(directory: &std::path::Path, limit: usize) -> io::Result<Usage> {
    let mut held = 0;
    for slot in 0..limit {
        let file = match File::open(directory.join(slot.to_string())) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => held += 1,
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
    Ok(Usage {
        limit,
        in_use: held,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, Invocation};
    use crate::testing::scaled;

    fn cluster() -> (tempfile::TempDir, Cluster) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cluster = Cluster::open(dir.path()).expect("cluster");
        (dir, cluster)
    }

    fn write(cluster: &Cluster, text: &str) {
        std::fs::write(Control::path(cluster), text).expect("daemon.toml");
    }

    #[test]
    fn without_a_file_nothing_is_limited() {
        let (_dir, cluster) = cluster();
        let control = Control::load(&cluster).expect("control");
        assert_eq!(control.agents.concurrency, None);
        let held: Vec<_> = (0..5)
            .map(|_| {
                control
                    .try_acquire(&cluster, Some("anyone"))
                    .expect("slots")
                    .expect("free")
            })
            .collect();
        assert_eq!(held.len(), 5);
        assert_eq!(control.usage(&cluster).expect("usage"), None);
    }

    #[test]
    fn pools_have_their_own_limits_and_share_the_daemons() {
        let (_dir, cluster) = cluster();
        write(
            &cluster,
            "[agents]\nconcurrency = 2\n\n[pool.specialized]\nagents = 1\n\n[pool.general]\nagents = 1\n",
        );
        let control = Control::load(&cluster).expect("control");
        let specialized = control
            .try_acquire(&cluster, Some("specialized"))
            .expect("slots")
            .expect("free");
        // The pool is full for its other explorers.
        assert!(
            control
                .try_acquire(&cluster, Some("specialized"))
                .expect("slots")
                .is_none()
        );
        // A refused invocation holds nothing of the daemon's limit.
        assert_eq!(
            control.usage(&cluster).expect("usage"),
            Some(Usage {
                limit: 2,
                in_use: 1
            })
        );
        // Another pool is not held up by it.
        let general = control
            .try_acquire(&cluster, Some("general"))
            .expect("slots")
            .expect("free");
        // The daemon's limit holds across pools, the unlimited included.
        assert!(
            control
                .try_acquire(&cluster, Some("unlimited"))
                .expect("slots")
                .is_none()
        );
        assert_eq!(
            control.pool_usage(&cluster).expect("usage")["specialized"],
            Usage {
                limit: 1,
                in_use: 1
            }
        );
        drop(specialized);
        drop(general);
        assert_eq!(
            control.usage(&cluster).expect("usage"),
            Some(Usage {
                limit: 2,
                in_use: 0
            })
        );
        assert!(
            control
                .try_acquire(&cluster, Some("specialized"))
                .expect("slots")
                .is_some()
        );
    }

    #[test]
    fn an_unreadable_control_file_is_waited_out() {
        let (_dir, cluster) = cluster();
        // `agent` is not a field: the file is invalid until it is fixed.
        write(&cluster, "[pool.semx]\nagent = 1\n");
        let never = Interrupt::default();
        let mut said = Vec::new();
        let slots = std::thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                acquire(&cluster, Some("semx"), &never, |why| {
                    said.push(why.to_owned());
                })
            });
            std::thread::sleep(POLL * 2);
            write(&cluster, "[pool.semx]\nagents = 1\n");
            waiting.join().expect("thread").expect("slot")
        });
        drop(slots);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("waiting for agent control:"),
            "{said:?}"
        );
    }

    #[test]
    fn a_waiting_invocation_stops_when_interrupted_and_sees_a_raised_limit() {
        let (_dir, cluster) = cluster();
        write(&cluster, "[agents]\nconcurrency = 1\n");
        let never = Interrupt::default();
        let held = acquire(&cluster, None, &never, |_| panic!("a slot was free")).expect("slot");

        let interrupt = Interrupt::default();
        let mut waited = 0;
        let error = std::thread::scope(|scope| {
            let waiting = scope.spawn(|| acquire(&cluster, None, &interrupt, |_| waited += 1));
            std::thread::sleep(POLL * 2);
            interrupt.raise();
            waiting.join().expect("thread").expect_err("interrupted")
        });
        assert!(matches!(error, AcquireError::Interrupted), "{error}");
        assert_eq!(waited, 1);

        // The operator raises the limit while an invocation waits.
        let second = std::thread::scope(|scope| {
            let waiting = scope.spawn(|| acquire(&cluster, None, &never, |_| {}));
            std::thread::sleep(POLL * 2);
            write(&cluster, "[agents]\nconcurrency = 2\n");
            waiting.join().expect("thread").expect("slot")
        });
        assert_eq!(
            Control::load(&cluster)
                .expect("control")
                .usage(&cluster)
                .expect("usage"),
            Some(Usage {
                limit: 2,
                in_use: 2
            })
        );
        drop((held, second));
    }

    /// Run `body` while `cluster` is served in this process: each queued
    /// command is supervised by a thread of its own, as a daemon gives
    /// each a process.
    fn served<T: Send>(cluster: &Cluster, body: impl FnOnce() -> T + Send) -> T {
        let daemon = Interrupt::default();
        std::thread::scope(|scope| {
            let serving = scope.spawn(|| {
                cluster.serve(
                    "local",
                    &daemon,
                    |_| Ok(()),
                    |_| Ok(()),
                    |job, peer| {
                        let (job, peer) = (job.to_owned(), peer.clone());
                        scope.spawn(move || cluster.run_job(&job, &peer, |_, command| Ok(command)));
                        std::process::Command::new("true").spawn()
                    },
                )
            });
            let deadline = std::time::Instant::now() + scaled(Duration::from_secs(20));
            while cluster.leader().expect("leader").is_none() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no leader was elected"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            let result = body();
            daemon.raise();
            serving.join().expect("daemon").expect("serve");
            result
        })
    }

    /// An agent of `pool` whose one CLI is the shell `script`.
    fn agent(cluster: &Cluster, pool: &str, script: &str) -> Agent {
        Agent::new(
            "scripted",
            BTreeMap::new(),
            vec![CliDeclaration {
                driver: DriverKind::Generic,
                command: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
                args: Vec::new(),
                env: BTreeMap::new(),
                timeout: None,
            }],
        )
        .expect("agent")
        .with_cluster(cluster.clone())
        .in_pool(pool)
    }

    /// Invoke `agent` once in `directory`.
    fn invoke(agent: &Agent, directory: &std::path::Path, log: &str) {
        let prompt = crate::Prompt::from("go");
        let schema = serde_json::json!({});
        let interrupt = Interrupt::default();
        agent
            .invoke::<serde_json::Value>(&Invocation {
                directory,
                prompt: &prompt,
                response_schema: &schema,
                facts: &[],
                log: &directory.join(log),
                interrupt: &interrupt,
                restore: None,
            })
            .expect("invocation");
    }

    /// Two agents invoked at once, each noting when its command starts and
    /// ends in `marks`; returns the marks in the order they were made.
    fn two_at_once(cluster: &Cluster, pools: [&str; 2], script: &str) -> Vec<String> {
        let work = tempfile::tempdir().expect("work");
        let script = script.replace("@DIR@", &work.path().display().to_string());
        served(cluster, || {
            std::thread::scope(|scope| {
                for (index, pool) in pools.into_iter().enumerate() {
                    let agent = agent(cluster, pool, &script);
                    let directory = work.path();
                    scope.spawn(move || invoke(&agent, directory, &format!("run-{index}.log")));
                }
            });
        });
        std::fs::read_to_string(work.path().join("marks"))
            .expect("marks")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Notes its start, lingers, notes its end.
    const LINGER: &str = "echo start >> @DIR@/marks; sleep 0.4; echo end >> @DIR@/marks; echo '{}'";

    #[test]
    fn agents_of_one_pool_never_run_at_once_under_its_limit() {
        let (_dir, cluster) = cluster();
        write(&cluster, "[pool.specialized]\nagents = 1\n");
        let marks = two_at_once(&cluster, ["specialized", "specialized"], LINGER);
        assert_eq!(marks, ["start", "end", "start", "end"]);
    }

    #[test]
    fn the_daemons_limit_holds_across_pools() {
        let (_dir, cluster) = cluster();
        write(
            &cluster,
            "[agents]\nconcurrency = 1\n\n[pool.a]\nagents = 1\n\n[pool.b]\nagents = 1\n",
        );
        let marks = two_at_once(&cluster, ["a", "b"], LINGER);
        assert_eq!(marks, ["start", "end", "start", "end"]);
    }

    #[test]
    fn pools_under_their_own_limits_run_side_by_side() {
        let (_dir, cluster) = cluster();
        write(
            &cluster,
            "[agents]\nconcurrency = 2\n\n[pool.a]\nagents = 1\n\n[pool.b]\nagents = 1\n",
        );
        // Each command ends only once both have started: they finish at
        // all only when they run at once.
        let marks = two_at_once(
            &cluster,
            ["a", "b"],
            "echo start >> @DIR@/marks; n=0; \
             while [ $(grep -c start @DIR@/marks) -lt 2 ] && [ $n -lt 200 ]; do \
               sleep 0.05; n=$((n+1)); done; \
             echo end >> @DIR@/marks; echo '{}'",
        );
        assert_eq!(marks, ["start", "start", "end", "end"]);
    }

    #[test]
    fn named_agents_are_validated() {
        let control = Control::parse(
            r#"
[agent.claude]
driver = "claude_code"
command = ["claude"]
args = ["--model", "claude-opus-5-5"]
timeout = "30m"

[[agent.claude.alternative]]
driver = "cursor_agent"
command = ["cursor-agent"]
"#,
        )
        .expect("control");
        let clis = control.agent["claude"].clis();
        assert_eq!(clis.len(), 2);
        assert_eq!(clis[0].driver, DriverKind::ClaudeCode);
        // An alternative without a timeout takes the agent's.
        assert_eq!(
            clis[1].timeout.as_ref().map(ToString::to_string).as_deref(),
            Some("30m")
        );

        for (text, problem) in [
            ("[agents]\nconcurrency = 0\n", "concurrency"),
            ("[agents]\nsurprise = 1\n", "unknown field"),
            ("[pool.x]\nagents = -1\n", "agents"),
            (
                "[agent.a]\ndriver = \"generic\"\ncommand = []\n",
                "`command` is empty",
            ),
            (
                "[agent.a]\ndriver = \"generic\"\ncommand = [\"x\"]\ntimeout = \"soon\"\n",
                "invalid `timeout`",
            ),
            ("[agent.a]\ncommand = [\"x\"]\n", "driver"),
        ] {
            let error = Control::parse(text).expect_err(problem);
            assert!(error.to_string().contains(problem), "{text}: {error}");
        }
    }
}
