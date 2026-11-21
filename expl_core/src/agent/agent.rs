//! [`Agent`] — a declared agent, and the invocation cycle it realises.
//!
//! An agent may be carried out by several CLIs (the primary one and its
//! `alternative`s). Each invocation is **routed**: it starts at the next
//! CLI in turn, so the work spreads across them, and falls over to the
//! following CLI when one fails (a usage limit, an outage, an unusable
//! reply, a declared `timeout` outrun). Every attempt goes to the same run
//! log.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::agent::{
    AgentDriver, AgentError, AgentRequest, AgentRun, ClaudeCodeDriver, CodexDriver,
    CursorAgentDriver, DriverKind, GenericDriver,
};
use crate::declaration::{AgentDeclaration, CliDeclaration, DeclarationError, TimeoutError};
use crate::process::Interrupt;
use crate::prompt::Prompt;
use crate::role::Role;

/// How long an interrupted or timed-out invocation gets to exit after
/// SIGTERM.
const TERMINATION_GRACE: Duration = Duration::from_secs(5);

/// One request to invoke an agent: what to say, where, and where to log.
#[derive(Debug, Clone, Copy)]
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
}

/// A declared agent. See the [module docs](crate::agent).
pub struct Agent {
    name: String,
    declaration: AgentDeclaration,
    clis: Vec<Cli>,
    /// Where the next invocation's route starts.
    turn: AtomicUsize,
}

/// One CLI that can carry out an agent.
struct Cli {
    driver: Box<dyn AgentDriver>,
    /// Environment of this CLI alone, over the agent's `env`.
    env: BTreeMap<String, String>,
    /// How long one attempt may run.
    timeout: Option<Duration>,
}

impl Cli {
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
            .map(crate::declaration::Timeout::duration)
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
        let drivers: Vec<DriverKind> = self.clis.iter().map(|c| c.driver.kind()).collect();
        f.debug_struct("Agent")
            .field("name", &self.name)
            .field("role", &self.declaration.role)
            .field("drivers", &drivers)
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Bring a declared agent to life: a mechanical field move into the
    /// drivers named by `driver` (and each `alternative`'s).
    ///
    /// # Errors
    ///
    /// [`DeclarationError::Timeout`] when a CLI's `timeout` is not a
    /// positive duration (a declaration that passed validation never has one).
    pub fn from_declaration(
        name: &str,
        declaration: &AgentDeclaration,
    ) -> Result<Self, DeclarationError> {
        let clis = declaration
            .clis()
            .into_iter()
            .map(Cli::from_declaration)
            .collect::<Result<_, _>>()
            .map_err(|source| DeclarationError::Timeout {
                agent: name.to_owned(),
                source,
            })?;
        Ok(Self {
            name: name.to_owned(),
            declaration: declaration.clone(),
            clis,
            // Processes of one agent start their routes at different CLIs.
            turn: AtomicUsize::new(std::process::id() as usize),
        })
    }

    /// Declared name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The role this agent plays.
    #[must_use]
    pub fn role(&self) -> Role {
        self.declaration.role
    }

    /// The declaration it was built from.
    #[must_use]
    pub fn declaration(&self) -> &AgentDeclaration {
        &self.declaration
    }

    /// Invoke the agent once and return its structured response: route the
    /// invocation to the CLI whose turn it is, falling over to the next CLI
    /// while one fails. An attempt that outruns its CLI's `timeout` is
    /// stopped and fails like any other.
    ///
    /// # Errors
    ///
    /// [`AgentError::Interrupted`] as soon as an attempt is interrupted;
    /// otherwise the error of the only CLI (such as
    /// [`AgentError::TimedOut`]), or [`AgentError::Failed`] naming every
    /// CLI's failure when each of several failed.
    pub fn invoke(&self, invocation: &Invocation<'_>) -> Result<AgentRun, AgentError> {
        let count = self.clis.len();
        let start = self.turn.fetch_add(1, Ordering::Relaxed) % count.max(1);
        let mut log = File::create(invocation.log)?;
        let mut failures = Vec::new();
        for offset in 0..count {
            let cli = &self.clis[(start + offset) % count];
            if offset > 0 {
                log = OpenOptions::new().append(true).open(invocation.log)?;
                writeln!(log, "=== falling over to the next CLI ===")?;
            }
            match self.attempt(cli, invocation, log.try_clone()?) {
                Ok(run) => return Ok(run),
                Err(AgentError::Interrupted) => return Err(AgentError::Interrupted),
                Err(error) if count == 1 => return Err(error),
                Err(error) => {
                    writeln!(log, "=== {} failed: {error} ===", cli.driver.kind().label())?;
                    failures.push(format!("{}: {error}", cli.driver.kind().label()));
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
        cli: &Cli,
        invocation: &Invocation<'_>,
        log: File,
    ) -> Result<AgentRun, AgentError> {
        let scratch = tempfile::Builder::new().prefix("expl-agent-").tempdir()?;
        let request = AgentRequest {
            prompt: invocation.prompt,
            response_schema: invocation.response_schema,
            scratch: scratch.path(),
            facts: invocation.facts,
        };
        let mut command = cli.driver.command(&request)?;
        let env: &BTreeMap<String, String> = &self.declaration.env;
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
            cli.driver.kind().label()
        )?;
        writeln!(log, "=== command: {:?}", command.process)?;
        writeln!(log, "=== prompt ===\n{}\n=== output ===", invocation.prompt)?;
        let log = Arc::new(Mutex::new(log));

        let mut child = command.process.spawn().map_err(|error| {
            AgentError::Launch(format!(
                "{}: {error}",
                command.process.get_program().display()
            ))
        })?;
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

        let end = wait(&mut child, invocation.interrupt, deadline.map(|(at, _)| at))?;
        let join = |handle: Option<thread::JoinHandle<std::io::Result<Vec<u8>>>>| {
            handle
                .map(|h| h.join().unwrap_or_else(|_| Ok(Vec::new())))
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
        feeder.join().unwrap_or(Ok(()))?;
        {
            let mut log = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match end {
                End::Exited(status) => writeln!(log, "=== exit: {status} ===")?,
                End::Interrupted => writeln!(log, "=== interrupted ===")?,
                End::TimedOut => writeln!(log, "=== timed out ===")?,
            }
        }
        match (end, deadline) {
            (End::Exited(_), _) => cli.driver.interpret(&output, &request),
            (End::TimedOut, Some((_, after))) => Err(AgentError::TimedOut { after }),
            (End::Interrupted | End::TimedOut, _) => Err(AgentError::Interrupted),
        }
    }
}

/// How waiting for a child ended.
#[derive(Debug, Clone, Copy)]
enum End {
    /// It exited by itself.
    Exited(ExitStatus),
    /// The hosting process was asked to stop, so the child was stopped.
    Interrupted,
    /// The deadline passed, so the child was stopped.
    TimedOut,
}

/// Wait for the child until it exits; on interrupt, or once `deadline`
/// passes, stop its whole process group (TERM, then KILL after a grace
/// period).
fn wait(
    child: &mut Child,
    interrupt: &Interrupt,
    deadline: Option<Instant>,
) -> std::io::Result<End> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(End::Exited(status));
        }
        let end = if interrupt.is_raised() {
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

/// Send `signal` to the process group led by `pid`.
fn signal_group(pid: u32, signal: &str) {
    // Best effort: the group may already be gone.
    Command::new("kill")
        .arg(signal)
        .arg("--")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok();
}

/// Copy a child pipe into the shared log line by line while buffering it.
fn tee(
    pipe: impl Read + Send + 'static,
    log: Arc<Mutex<File>>,
    prefix: &'static str,
) -> thread::JoinHandle<std::io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut captured = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            captured.extend_from_slice(&line);
            let mut log = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            log.write_all(prefix.as_bytes())?;
            log.write_all(&line)?;
            if !line.ends_with(b"\n") {
                log.write_all(b"\n")?;
            }
        }
        Ok(captured)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declaration::Declaration;

    fn agent(text: &str) -> Agent {
        let declaration = Declaration::parse(text).expect("declaration");
        Agent::from_declaration("a", declaration.agent("a").expect("agent")).expect("agent")
    }

    fn invoke(agent: &Agent, log: &Path) -> Result<AgentRun, AgentError> {
        let prompt = Prompt::from("p");
        let schema = serde_json::json!({});
        agent.invoke(&Invocation {
            directory: Path::new("."),
            prompt: &prompt,
            response_schema: &schema,
            facts: &[],
            log,
            interrupt: &Interrupt::default(),
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
        assert!(
            started.elapsed() < Duration::from_secs(5),
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
        let gone_by = Instant::now() + Duration::from_secs(3);
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
                started.elapsed() < Duration::from_secs(5),
                "{:?}",
                started.elapsed()
            );
            let text = std::fs::read_to_string(&log).expect("log");
            fell_over |= text.contains("generic failed: agent timed out after 1s")
                && text.contains("falling over to the next CLI");
        }
        assert!(fell_over, "no invocation fell over from the timed-out CLI");
    }

    fn agent_text_all_failing() -> String {
        ROUTED
            .replace("echo '{\\\"cli\\\":1}'", "exit 3")
            .replace("echo '{\\\"cli\\\":2}'", "exit 4")
    }
}
