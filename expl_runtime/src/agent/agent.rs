//! [`Agent`] — invoke a command using the handler's prompt and response contract.
//!
//! An agent may be carried out by several CLIs (the primary one and its
//! `alternative`s). Each invocation is **routed**: it starts at the next
//! CLI in turn, so the work spreads across them, and falls over to the
//! following CLI when one fails (a usage limit, an outage, an unusable
//! reply, a reply that does not decode into the response the handler expects,
//! a declared `timeout` outrun). Every attempt goes to the same run log.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use opentelemetry::trace::TraceContextExt;
use opentelemetry::{Context, KeyValue};
use serde::de::DeserializeOwned;

use crate::Interrupt;
use crate::agent::supervisor::{End, Reaped, reap_group, wait};
use crate::agent::{
    AgentDriver, AgentError, AgentRequest, AgentRun, ClaudeCodeDriver, CodexDriver,
    CursorAgentDriver, DriverKind, GenericDriver,
};
use crate::cluster::{Cluster, CommandSpec, Outcome};
use crate::command::{CliDeclaration, TimeoutError};
use crate::prompt::Prompt;
use crate::telemetry;

/// How long the output is still read once the CLI and its process group are
/// gone. A process that escaped the group can hold the pipes open forever;
/// past this bound, whatever was read so far is the output.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// One request to invoke an agent: what to say, where, and where to log.
#[derive(Clone, Copy)]
pub struct Invocation<'a> {
    /// The directory the agent works in (a worktree).
    pub directory: &'a Path,
    /// The rendered instructions.
    pub prompt: &'a Prompt,
    /// JSON Schema the response must satisfy.
    pub response_schema: &'a serde_json::Value,
    /// What the reaction is about (`EXPL_ROLE`, `EXPL_NODE`, …), for
    /// wrapper commands. Only the generic driver passes it on; an AI CLI is
    /// never told it runs under expl.
    pub facts: &'a [(String, String)],
    /// File receiving the prompt and the teed raw output.
    pub log: &'a Path,
    /// Raised when the hosting process must stop.
    pub interrupt: &'a Interrupt,
    /// Puts `directory` back as the first CLI found it, before the next CLI
    /// starts on it: whatever a failed attempt left behind is not the next
    /// attempt's starting point.
    pub restore: Option<&'a dyn Fn() -> std::io::Result<()>>,
}

impl std::fmt::Debug for Invocation<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invocation")
            .field("directory", &self.directory)
            .field("log", &self.log)
            .field("restore", &self.restore.is_some())
            .finish_non_exhaustive()
    }
}

/// Command runner for a reaction. It has no subscription or graph state.
pub struct Agent {
    name: String,
    env: BTreeMap<String, String>,
    clis: Vec<DriverConfig>,
    /// Where the next invocation's route starts.
    turn: AtomicUsize,
    cluster: Option<Cluster>,
}

/// One driver, with its own environment and attempt timeout.
pub struct DriverConfig {
    /// Converts input and output for this runner.
    pub driver: Box<dyn AgentDriver>,
    /// Environment of this CLI alone, over the agent's `env`.
    pub env: BTreeMap<String, String>,
    /// How long one attempt may run.
    pub timeout: Option<Duration>,
}

impl DriverConfig {
    fn from_declaration(declaration: CliDeclaration) -> Result<Self, TimeoutError> {
        let CliDeclaration {
            driver,
            command,
            args,
            env,
            timeout,
        } = declaration;
        let timeout = timeout
            .as_ref()
            .map(crate::command::Timeout::duration)
            .transpose()?;
        let driver: Box<dyn AgentDriver> = match driver {
            DriverKind::ClaudeCode => Box::new(ClaudeCodeDriver::new(command, args)),
            DriverKind::Codex => Box::new(CodexDriver::new(command, args)),
            DriverKind::CursorAgent => Box::new(CursorAgentDriver::new(command, args)),
            DriverKind::Generic => Box::new(GenericDriver::new(command, args)),
        };
        Ok(Self {
            driver,
            env,
            timeout,
        })
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let drivers: Vec<_> = self.clis.iter().map(|c| c.driver.label()).collect();
        f.debug_struct("Agent")
            .field("name", &self.name)
            .field("drivers", &drivers)
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Build the runner for a consumer's commands in rotation order.
    ///
    /// # Errors
    ///
    /// [`TimeoutError`] when a CLI's timeout is invalid.
    pub fn new(
        name: &str,
        env: BTreeMap<String, String>,
        clis: Vec<CliDeclaration>,
    ) -> Result<Self, TimeoutError> {
        let clis = clis
            .into_iter()
            .map(DriverConfig::from_declaration)
            .collect::<Result<_, _>>()?;
        Ok(Self::from_drivers(name, env, clis))
    }

    /// Build an agent using application-provided drivers.
    #[must_use]
    pub fn from_drivers(
        name: &str,
        env: BTreeMap<String, String>,
        clis: Vec<DriverConfig>,
    ) -> Self {
        Self {
            name: name.to_owned(),
            env,
            clis,
            // Replicas start their routes at different CLIs.
            turn: AtomicUsize::new(std::process::id() as usize),
            cluster: None,
        }
    }

    /// Route command attempts through cluster members.
    #[must_use]
    pub fn with_cluster(mut self, cluster: Cluster) -> Self {
        self.cluster = Some(cluster);
        self
    }

    /// Invoke the agent once and return its structured response decoded
    /// into `R`: route the invocation to the CLI whose turn it is, falling
    /// over to the next CLI while one fails. An attempt that outruns its
    /// CLI's `timeout` is stopped and fails like any other, and so does one
    /// whose reply does not decode into `R`.
    ///
    /// # Errors
    ///
    /// [`AgentError::Interrupted`] as soon as an attempt is interrupted,
    /// and [`AgentError::Lingering`] as soon as an attempt leaves processes
    /// behind that cannot be stopped; otherwise the error of the only CLI (such as
    /// [`AgentError::TimedOut`] or [`AgentError::Mismatch`]), or
    /// [`AgentError::Failed`] naming every CLI's failure when each of
    /// several failed.
    pub fn invoke<R: DeserializeOwned>(
        &self,
        invocation: &Invocation<'_>,
    ) -> Result<AgentRun<R>, AgentError> {
        self.invoke_validated(invocation, |_| Ok(()))
    }

    /// Invoke the agent, treating a response that fails `validate` as a
    /// failed CLI attempt, just like a response that does not decode.
    ///
    /// # Errors
    ///
    /// As in [`Self::invoke`], including validation failures.
    pub fn invoke_validated<R: DeserializeOwned>(
        &self,
        invocation: &Invocation<'_>,
        validate: impl Fn(&R) -> Result<(), AgentError>,
    ) -> Result<AgentRun<R>, AgentError> {
        let count = self.clis.len();
        let start = self.turn.fetch_add(1, Ordering::Relaxed) % count.max(1);
        let mut log = File::create(invocation.log)?;
        let mut failures = Vec::new();
        for offset in 0..count {
            let cli = &self.clis[(start + offset) % count];
            if offset > 0 {
                log = OpenOptions::new().append(true).open(invocation.log)?;
                if let Some(restore) = invocation.restore {
                    restore()?;
                    writeln!(log, "=== restored the working directory ===")?;
                }
                writeln!(log, "=== falling over to the next CLI ===")?;
            }
            let attempt_log = log.try_clone()?;
            let context = telemetry::span(
                "expl.agent.attempt",
                vec![
                    KeyValue::new("expl.agent.driver", cli.driver.label()),
                    KeyValue::new("expl.agent.attempt", (offset + 1) as i64),
                    KeyValue::new("expl.agent.fallback", offset > 0),
                ],
            );
            if let Some(timeout) = cli.timeout {
                context.span().set_attribute(KeyValue::new(
                    "expl.agent.timeout_seconds",
                    timeout.as_secs_f64(),
                ));
            }
            let result = {
                let _scope = context.clone().attach();
                self.attempt(cli, invocation, attempt_log)
                    .and_then(AgentRun::decode)
                    .and_then(|run| {
                        validate(&run.response)?;
                        Ok(run)
                    })
            };
            if let Err(error) = &result {
                telemetry::error(&context, error.telemetry_kind());
            }
            context.span().end();
            match result {
                Ok(run) => return Ok(run),
                Err(AgentError::Interrupted) => return Err(AgentError::Interrupted),
                // The directory is still in use: no other CLI may start on it.
                Err(error @ (AgentError::Lingering { .. } | AgentError::Transport(_))) => {
                    return Err(error);
                }
                Err(error) if count == 1 => return Err(error),
                Err(error) => {
                    writeln!(log, "=== {} failed: {error} ===", cli.driver.label())?;
                    failures.push(format!("{}: {error}", cli.driver.label()));
                }
            }
        }
        Err(AgentError::Failed {
            code: None,
            message: format!("every CLI failed — {}", failures.join("; ")),
        })
    }

    /// One attempt with `cli`, logged to `log`.
    fn attempt(
        &self,
        cli: &DriverConfig,
        invocation: &Invocation<'_>,
        log: File,
    ) -> Result<AgentRun, AgentError> {
        let scratch = match &self.cluster {
            Some(cluster) => tempfile::Builder::new()
                .prefix("expl-agent-")
                .tempdir_in(cluster.root())?,
            None => tempfile::Builder::new().prefix("expl-agent-").tempdir()?,
        };
        let request = AgentRequest {
            prompt: invocation.prompt,
            response_schema: invocation.response_schema,
            scratch: scratch.path(),
            facts: invocation.facts,
        };
        let mut command = cli.driver.command(&request)?;
        let env: &BTreeMap<String, String> = &self.env;
        command
            .process
            .current_dir(invocation.directory)
            .envs(env)
            .envs(&cli.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group: a terminal Ctrl-C reaches expl, which then
            // stops the whole agent tree itself.
            .process_group(0);

        let mut log = log;
        writeln!(
            log,
            "=== agent `{}` ({}) ===",
            self.name,
            cli.driver.label()
        )?;
        writeln!(log, "=== command: {:?}", command.process)?;
        writeln!(log, "=== prompt ===\n{}\n=== output ===", invocation.prompt)?;
        if let Some(cluster) = &self.cluster {
            let spec = CommandSpec::capture(&command.process, invocation.directory);
            let mut stdout = RemoteLog::new(log.try_clone()?, "");
            let mut stderr = RemoteLog::new(log.try_clone()?, "[stderr] ");
            let result = cluster.execute(
                spec,
                command.stdin.as_deref(),
                cli.timeout,
                invocation.interrupt,
                (&mut stdout, &mut stderr),
            );
            let stdout = stdout.finish()?;
            let stderr = stderr.finish()?;
            let status = match result.map_err(AgentError::Transport)? {
                Outcome::Exited(status) => {
                    let status = ExitStatus::from_raw(status);
                    writeln!(log, "=== exit: {status} ===")?;
                    status
                }
                Outcome::TimedOut => {
                    writeln!(log, "=== timed out ===")?;
                    return Err(AgentError::TimedOut {
                        after: cli.timeout.unwrap_or_default(),
                    });
                }
                Outcome::Interrupted => {
                    writeln!(log, "=== interrupted ===")?;
                    return Err(AgentError::Interrupted);
                }
                Outcome::Lingering { group } => return Err(AgentError::Lingering { group }),
                Outcome::Failed(error) => return Err(AgentError::Launch(error)),
            };
            let output = std::process::Output {
                status,
                stdout,
                stderr,
            };
            return cli.driver.interpret(&output, &request);
        }
        let log = Arc::new(Mutex::new(log));

        let mut child = command.process.spawn().map_err(|error| {
            AgentError::Launch(format!(
                "{}: {error}",
                command.process.get_program().display()
            ))
        })?;
        let telemetry = Context::current();
        telemetry
            .span()
            .set_attribute(KeyValue::new("expl.agent.pid", i64::from(child.id())));
        let deadline = cli
            .timeout
            .map(|timeout| (Instant::now() + timeout, timeout));

        let stdin = child.stdin.take();
        let payload = command.stdin.take();
        let feeder = thread::spawn(move || -> std::io::Result<()> {
            if let (Some(mut pipe), Some(text)) = (stdin, payload) {
                match pipe.write_all(text.as_bytes()) {
                    Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
                        return Err(error);
                    }
                    Ok(()) | Err(_) => {}
                }
            }
            Ok(())
        });
        let stdout = child
            .stdout
            .take()
            .map(|pipe| tee(pipe, Arc::clone(&log), ""));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| tee(pipe, Arc::clone(&log), "[stderr] "));

        let end = wait(
            &mut child,
            || invocation.interrupt.is_raised(),
            deadline.map(|(at, _)| at),
        )?;
        if let End::Exited(status) = end
            && let Some(code) = status.code()
        {
            telemetry
                .span()
                .set_attribute(KeyValue::new("process.exit.code", i64::from(code)));
        }
        // However the leader ended (by itself, or stopped on a timeout or an
        // interrupt), members of its group may outlive it: stop them and see
        // them gone before the working directory goes to the next CLI or to
        // whichever process takes the work over.
        let reaped = reap_group(child.id());
        let drained_by = Instant::now() + DRAIN_GRACE;
        let join = |tee: Option<Tee>| {
            tee.map(|tee| tee.finish(drained_by))
                .transpose()
                .map(Option::unwrap_or_default)
        };
        let output = std::process::Output {
            status: match end {
                End::Exited(status) => status,
                End::Interrupted | End::TimedOut => ExitStatus::default(),
            },
            stdout: join(stdout)?,
            stderr: join(stderr)?,
        };
        if let Some(fed) = join_by(feeder, drained_by) {
            fed.unwrap_or(Ok(()))?;
        }
        {
            let mut log = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match reaped {
                Reaped::Empty => {}
                Reaped::Stopped => writeln!(
                    log,
                    "=== note: the CLI left processes behind; its process group was killed ==="
                )?,
                Reaped::Lingering => writeln!(
                    log,
                    "=== note: the CLI left processes behind that survived SIGKILL ==="
                )?,
            }
            match end {
                End::Exited(status) => writeln!(log, "=== exit: {status} ===")?,
                End::Interrupted => writeln!(log, "=== interrupted ===")?,
                End::TimedOut => writeln!(log, "=== timed out ===")?,
            }
        }
        if reaped == Reaped::Lingering {
            return Err(AgentError::Lingering { group: child.id() });
        }
        match (end, deadline) {
            (End::Exited(_), _) => cli.driver.interpret(&output, &request),
            (End::TimedOut, Some((_, after))) => Err(AgentError::TimedOut { after }),
            (End::Interrupted | End::TimedOut, _) => Err(AgentError::Interrupted),
        }
    }
}

/// Join `handle` if its thread finishes by `deadline`; otherwise leave it
/// running detached and return `None`.
fn join_by<T>(handle: thread::JoinHandle<T>, deadline: Instant) -> Option<thread::Result<T>> {
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
    Some(handle.join())
}

/// A thread copying a child pipe into the shared log, and what it has
/// captured so far.
struct Tee {
    handle: thread::JoinHandle<std::io::Result<()>>,
    captured: Arc<Mutex<Vec<u8>>>,
}

impl Tee {
    /// The captured output once the pipe closes, or what was captured by
    /// `deadline` if it is still held open then.
    fn finish(self, deadline: Instant) -> std::io::Result<Vec<u8>> {
        if let Some(Ok(result)) = join_by(self.handle, deadline) {
            result?;
        }
        let mut captured = self
            .captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(std::mem::take(&mut *captured))
    }
}

/// Copy a child pipe into the shared log line by line while buffering it.
fn tee(pipe: impl Read + Send + 'static, log: Arc<Mutex<File>>, prefix: &'static str) -> Tee {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let buffer = Arc::clone(&captured);
    let handle = thread::spawn(move || {
        let mut pipe = pipe;
        let mut chunk = [0_u8; 8192];
        let mut pending = Vec::new();
        let write_line = |line: &[u8]| -> std::io::Result<()> {
            let mut log = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            log.write_all(prefix.as_bytes())?;
            log.write_all(line)?;
            if !line.ends_with(b"\n") {
                log.write_all(b"\n")?;
            }
            Ok(())
        };
        loop {
            let read = match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(&chunk[..read]);
            pending.extend_from_slice(&chunk[..read]);
            while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                write_line(&pending[..=end])?;
                pending.drain(..=end);
            }
        }
        if !pending.is_empty() {
            write_line(&pending)?;
        }
        Ok(())
    });
    Tee { handle, captured }
}

/// Tee a remote output file as it grows, preserving the bytes drivers need.
struct RemoteLog {
    log: File,
    prefix: &'static str,
    bytes: Vec<u8>,
    logged: usize,
}

impl RemoteLog {
    fn new(log: File, prefix: &'static str) -> Self {
        Self {
            log,
            prefix,
            bytes: Vec::new(),
            logged: 0,
        }
    }

    fn finish(mut self) -> std::io::Result<Vec<u8>> {
        if self.logged < self.bytes.len() {
            write!(self.log, "{}", self.prefix)?;
            self.log.write_all(&self.bytes[self.logged..])?;
            self.log.write_all(b"\n")?;
        }
        Ok(self.bytes)
    }
}

impl Write for RemoteLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        while let Some(end) = self.bytes[self.logged..]
            .iter()
            .position(|&byte| byte == b'\n')
        {
            let end = self.logged + end + 1;
            self.log.write_all(self.prefix.as_bytes())?;
            self.log.write_all(&self.bytes[self.logged..end])?;
            self.logged = end;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.log.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::supervisor::{LEFTOVER_GRACE, TERMINATION_GRACE};
    use crate::testing::scaled;
    use std::process::Command;

    /// The longest a CLI with a `timeout` of 1s may take to be stopped
    /// when it goes on the first TERM: its timeout, plus the
    /// [`TERMINATION_GRACE`] it must not wait out. A stop that does wait it
    /// out fails this at scale 1; the scale only widens the slack.
    fn stopped_within() -> Duration {
        Duration::from_secs(1) + scaled(TERMINATION_GRACE)
    }

    fn agent(text: &str) -> Agent {
        let mut definition: toml::Value = toml::from_str(text).expect("declaration");
        let consumer = definition["agent"]["a"].as_table_mut().expect("consumer");
        let alternatives = consumer.remove("alternative");
        let env = consumer
            .remove("env")
            .map_or_else(BTreeMap::new, |env| env.try_into().expect("env"));
        consumer.remove("role");
        consumer.remove("replicas");
        let mut primary: CliDeclaration = toml::Value::Table(consumer.clone())
            .try_into()
            .expect("primary command");
        let mut alternatives: Vec<CliDeclaration> =
            alternatives.map_or_else(Vec::new, |value| value.try_into().expect("alternatives"));
        for alternative in &mut alternatives {
            alternative.timeout = alternative
                .timeout
                .take()
                .or_else(|| primary.timeout.clone());
        }
        primary.env = BTreeMap::new();
        Agent::new(
            "a",
            env,
            std::iter::once(primary).chain(alternatives).collect(),
        )
        .expect("runner")
    }

    fn invoke(agent: &Agent, log: &Path) -> Result<AgentRun, AgentError> {
        invoke_as(agent, log)
    }

    fn invoke_as<R: DeserializeOwned>(
        agent: &Agent,
        log: &Path,
    ) -> Result<AgentRun<R>, AgentError> {
        let prompt = Prompt::from("p");
        let schema = serde_json::json!({});
        agent.invoke(&Invocation {
            directory: Path::new("."),
            prompt: &prompt,
            response_schema: &schema,
            facts: &[],
            log,
            interrupt: &Interrupt::default(),
            restore: None,
        })
    }

    const ROUTED: &str = r#"
[agent.a]
role = "planning"
driver = "generic"
command = ["sh", "-c", "echo '{\"cli\":1}'"]
replicas = 1

[[agent.a.alternative]]
driver = "generic"
command = ["sh", "-c", "echo '{\"cli\":2}'"]
"#;

    #[test]
    fn invocations_take_turns_across_clis() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("run.log");
        let agent = agent(ROUTED);
        let mut seen: Vec<serde_json::Value> = (0..4)
            .map(|_| invoke(&agent, &log).expect("run").response["cli"].clone())
            .collect();
        seen.sort_by_key(serde_json::Value::as_i64);
        assert_eq!(seen, [1, 1, 2, 2].map(serde_json::Value::from));
    }

    #[test]
    fn the_next_cli_starts_from_a_restored_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("run.log");
        let agent = agent(&ROUTED.replacen("echo '{\\\"cli\\\":1}'", "exit 3", 1));
        let restored = std::cell::Cell::new(0);
        let restore = || {
            restored.set(restored.get() + 1);
            Ok(())
        };
        let prompt = Prompt::from("p");
        let schema = serde_json::json!({});
        for _ in 0..2 {
            let run: AgentRun<serde_json::Value> = agent
                .invoke(&Invocation {
                    directory: Path::new("."),
                    prompt: &prompt,
                    response_schema: &schema,
                    facts: &[],
                    log: &log,
                    interrupt: &Interrupt::default(),
                    restore: Some(&restore),
                })
                .expect("run");
            assert_eq!(run.response["cli"], 2);
        }
        // Only the invocation that started at the failing CLI fell over.
        assert_eq!(restored.get(), 1);
    }

    #[test]
    fn members_ignoring_term_die_with_a_stopped_cli() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("run.log");
        let pid_file = dir.path().join("pid");
        let text = format!(
            r#"
[agent.a]
role = "planning"
driver = "generic"
command = ["sh", "-c", "(trap '' TERM; exec sleep 60) & echo $! > {}; sleep 30"]
replicas = 1
timeout = 1
"#,
            pid_file.display()
        );
        let error = invoke(&agent(&text), &log).expect_err("times out");
        assert!(matches!(error, AgentError::TimedOut { .. }), "{error:?}");
        let member = std::fs::read_to_string(&pid_file).expect("pid");
        let alive = || {
            Command::new("kill")
                .args(["-0", member.trim()])
                .stderr(Stdio::null())
                .status()
                .expect("kill")
                .success()
        };
        let gone_by = Instant::now() + scaled(Duration::from_secs(3));
        while alive() && Instant::now() < gone_by {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(), "{} ignored TERM and survived", member.trim());
    }

    #[test]
    fn a_failing_cli_falls_over_to_the_next() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("run.log");
        let routed = agent(&ROUTED.replacen("echo '{\\\"cli\\\":1}'", "exit 3", 1));
        for _ in 0..2 {
            let run = invoke(&routed, &log).expect("run");
            assert_eq!(run.response["cli"], 2);
        }
        let failing = agent_text_all_failing();
        let error = invoke(&agent(&failing), &log).expect_err("all fail");
        assert!(error.to_string().contains("every CLI failed"), "{error}");
    }

    #[derive(Debug, serde::Deserialize)]
    struct Typed {
        cli: i64,
    }

    #[test]
    fn a_response_of_the_wrong_shape_falls_over_to_the_next() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("run.log");
        let routed = agent(&ROUTED.replacen(
            "echo '{\\\"cli\\\":1}'",
            "echo '{\\\"cli\\\":\\\"one\\\"}'",
            1,
        ));
        let mut fell_over = false;
        // Routing alternates the first CLI, so one of two starts at it.
        for _ in 0..2 {
            let run = invoke_as::<Typed>(&routed, &log).expect("run");
            assert_eq!(run.response.cli, 2);
            let text = std::fs::read_to_string(&log).expect("log");
            fell_over |= text.contains(r#"{"cli":"one"}"#)
                && text.contains("generic failed: agent response does not match its schema")
                && text.contains("falling over to the next CLI");
        }
        assert!(
            fell_over,
            "no invocation fell over from the invalid response"
        );

        let single = agent(
            r#"
[agent.a]
role = "planning"
driver = "generic"
command = ["sh", "-c", "echo '{\"other\":1}'"]
replicas = 1
"#,
        );
        let error = invoke_as::<Typed>(&single, &log).expect_err("missing field");
        assert!(matches!(error, AgentError::Mismatch(_)), "{error:?}");
        assert!(error.to_string().contains("missing field `cli`"), "{error}");
    }

    #[test]
    fn a_cli_outrunning_its_timeout_is_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("run.log");
        let pid_file = dir.path().join("pid");
        let text = format!(
            r#"
[agent.a]
role = "planning"
driver = "generic"
command = ["sh", "-c", "echo $$ > {}; sleep 30"]
replicas = 1
timeout = 1
"#,
            pid_file.display()
        );
        let started = Instant::now();
        let error = invoke(&agent(&text), &log).expect_err("times out");
        assert!(
            matches!(error, AgentError::TimedOut { after } if after == Duration::from_secs(1)),
            "{error:?}"
        );
        assert!(error.to_string().contains("timed out after 1s"), "{error}");
        // Stopped as the timeout passed, not after `sleep 30`, and without
        // waiting out the TERM grace once the group went.
        assert!(
            started.elapsed() < stopped_within(),
            "{:?}",
            started.elapsed()
        );
        assert!(
            std::fs::read_to_string(&log)
                .expect("log")
                .contains("=== timed out ==="),
        );

        // The CLI led its own process group: once stopped, nothing of it
        // (neither `sh` nor its `sleep`) is left.
        let group = std::fs::read_to_string(&pid_file).expect("pid");
        let alive = || {
            Command::new("kill")
                .args(["-0", "--", &format!("-{}", group.trim())])
                .stderr(Stdio::null())
                .status()
                .expect("kill")
                .success()
        };
        let gone_by = Instant::now() + scaled(Duration::from_secs(3));
        while alive() && Instant::now() < gone_by {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(), "process group {} survived", group.trim());
    }

    #[test]
    fn a_timed_out_cli_falls_over_to_the_next() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("run.log");
        let routed = agent(
            &ROUTED
                .replacen("echo '{\\\"cli\\\":1}'", "sleep 30", 1)
                .replacen("replicas = 1", "replicas = 1\ntimeout = 1", 1)
                .replacen(
                    "[[agent.a.alternative]]",
                    "[[agent.a.alternative]]\ntimeout = 30",
                    1,
                ),
        );
        let mut fell_over = false;
        // Routing alternates the first CLI, so one of two starts at `sleep`.
        for _ in 0..2 {
            let started = Instant::now();
            let run = invoke(&routed, &log).expect("run");
            assert_eq!(run.response["cli"], 2);
            assert!(
                started.elapsed() < stopped_within(),
                "{:?}",
                started.elapsed()
            );
            let text = std::fs::read_to_string(&log).expect("log");
            fell_over |= text.contains("generic failed: agent timed out after 1s")
                && text.contains("falling over to the next CLI");
        }
        assert!(fell_over, "no invocation fell over from the timed-out CLI");
    }

    #[test]
    fn background_children_holding_the_pipes_are_reaped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("run.log");
        let pid_file = dir.path().join("pid");
        let text = format!(
            r#"
[agent.a]
role = "planning"
driver = "generic"
command = ["sh", "-c", "echo '{{\"ok\":true}}'; sleep 60 & echo $! > {}"]
replicas = 1
"#,
            pid_file.display()
        );
        let started = Instant::now();
        let run = invoke(&agent(&text), &log).expect("run");
        assert_eq!(run.response["ok"], true);
        // The child holding the pipes was reaped before the output was
        // drained, so the drain grace was not waited out.
        assert!(
            started.elapsed() < scaled(DRAIN_GRACE),
            "{:?}",
            started.elapsed()
        );
        assert!(
            std::fs::read_to_string(&log)
                .expect("log")
                .contains("its process group was killed"),
        );

        let sleeper = std::fs::read_to_string(&pid_file).expect("pid");
        let alive = || {
            Command::new("kill")
                .args(["-0", sleeper.trim()])
                .stderr(Stdio::null())
                .status()
                .expect("kill")
                .success()
        };
        let gone_by = Instant::now() + scaled(Duration::from_secs(3));
        while alive() && Instant::now() < gone_by {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(), "background child {} survived", sleeper.trim());
    }

    /// Write a CLI script into `dir` whose background child ignores TERM
    /// and holds the CLI's stdout open, while the CLI itself waits in the
    /// foreground until it is stopped. `member` receives the child's pid and,
    /// on the next line, the CLI's own: the process group they share.
    fn stubborn_cli(dir: &Path, member: &Path) -> std::path::PathBuf {
        let script = dir.join("stubborn.sh");
        std::fs::write(
            &script,
            format!(
                "(trap '' TERM; exec sleep 60) &\n\
                 printf '%s\\n%s\\n' $! $$ > {m}.tmp\n\
                 mv {m}.tmp {m}\n\
                 sleep 30\n",
                m = member.display()
            ),
        )
        .expect("script");
        script
    }

    /// Whether `target` exists: a pid, or `-pid` for any member of that
    /// process group (a zombie not reaped yet included).
    fn exists(target: &str) -> bool {
        Command::new("kill")
            .args(["-0", "--", target])
            .stderr(Stdio::null())
            .status()
            .expect("kill")
            .success()
    }

    #[test]
    fn a_timed_out_cli_is_reaped_before_the_next_cli_starts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("run.log");
        let member = dir.path().join("member");
        let stubborn = stubborn_cli(dir.path(), &member);
        // The next CLI reports, as it starts, whether anything of the
        // timed-out CLI's process group (the member it left behind, even
        // as a zombie) is still there.
        let probe = dir.path().join("probe.sh");
        std::fs::write(
            &probe,
            format!(
                "if [ -f {m} ]; then\n\
                 \x20 group=$(sed -n 2p {m}); rm {m}\n\
                 \x20 if kill -0 -- \"-$group\" 2>/dev/null; then a=true; else a=false; fi\n\
                 \x20 echo \"{{\\\"alive\\\":$a}}\"\n\
                 else\n\
                 \x20 echo '{{\"alive\":null}}'\n\
                 fi\n",
                m = member.display()
            ),
        )
        .expect("probe");
        let routed = agent(&format!(
            r#"
[agent.a]
role = "planning"
driver = "generic"
command = ["sh", "{}"]
replicas = 1
timeout = 1

[[agent.a.alternative]]
driver = "generic"
command = ["sh", "{}"]
"#,
            stubborn.display(),
            probe.display()
        ));
        let mut fell_over = 0;
        // Routing alternates the first CLI, so one of two starts at it.
        for _ in 0..2 {
            let run = invoke(&routed, &log).expect("run");
            if let Some(alive) = run.response["alive"].as_bool() {
                assert!(!alive, "the next CLI started beside the timed-out group");
                let text = std::fs::read_to_string(&log).expect("log");
                assert!(text.contains("generic failed: agent timed out"), "{text}");
                assert!(text.contains("its process group was killed"), "{text}");
                fell_over += 1;
            }
        }
        assert_eq!(fell_over, 1, "no invocation fell over from the timeout");
    }

    #[test]
    fn an_interrupted_cli_is_reaped_before_the_invocation_returns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("run.log");
        let member = dir.path().join("member");
        let stubborn = stubborn_cli(dir.path(), &member);
        let agent = agent(&format!(
            r#"
[agent.a]
role = "planning"
driver = "generic"
command = ["sh", "{}"]
replicas = 1
"#,
            stubborn.display()
        ));
        let interrupt = Interrupt::default();
        let raiser = {
            let interrupt = interrupt.clone();
            let member = member.clone();
            thread::spawn(move || {
                let by = Instant::now() + scaled(Duration::from_secs(10));
                while !member.exists() && Instant::now() < by {
                    thread::sleep(Duration::from_millis(20));
                }
                interrupt.raise();
                Instant::now()
            })
        };
        let prompt = Prompt::from("p");
        let schema = serde_json::json!({});
        let error = agent
            .invoke::<serde_json::Value>(&Invocation {
                directory: dir.path(),
                prompt: &prompt,
                response_schema: &schema,
                facts: &[],
                log: &log,
                interrupt: &interrupt,
                restore: None,
            })
            .expect_err("interrupted");
        let returned = Instant::now();
        let interrupted_at = raiser.join().expect("raiser");
        assert!(matches!(error, AgentError::Interrupted), "{error:?}");
        // Gone by the time the invocation returns, not some time after:
        // whoever takes the work over may start on the directory at once.
        let pids = std::fs::read_to_string(&member).expect("pids");
        let (pid, group) = pids.trim().split_once('\n').expect("two pids");
        assert!(!exists(pid), "member {pid} outlived the invocation");
        assert!(
            !exists(&format!("-{group}")),
            "process group {group} outlived the invocation"
        );
        // The member held stdout, yet the output was not waited on for
        // the drain grace: from the interrupt, only the leftover grace the
        // member spent ignoring TERM was. Draining before the member is
        // gone waits out both, and fails this at scale 1.
        let took = returned.duration_since(interrupted_at);
        assert!(
            took < LEFTOVER_GRACE + scaled(DRAIN_GRACE),
            "{took:?} from the interrupt"
        );
        let text = std::fs::read_to_string(&log).expect("log");
        assert!(text.contains("=== interrupted ==="), "{text}");
        assert!(text.contains("its process group was killed"), "{text}");
    }

    fn agent_text_all_failing() -> String {
        ROUTED
            .replace("echo '{\\\"cli\\\":1}'", "exit 3")
            .replace("echo '{\\\"cli\\\":2}'", "exit 4")
    }
}
