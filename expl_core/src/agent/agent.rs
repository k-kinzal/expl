//! [`Agent`] — a declared agent, and the invocation cycle it realises.
//!
//! An agent may be carried out by several CLIs (the primary one and its
//! `alternative`s). Each invocation is **routed**: it starts at the next
//! CLI in turn, so the work spreads across them, and falls over to the
//! following CLI when one fails (a usage limit, an outage, an unusable
//! reply). Every attempt goes to the same run log.

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
use crate::declaration::{AgentDeclaration, CliDeclaration};
use crate::process::Interrupt;
use crate::prompt::Prompt;
use crate::role::Role;

/// How long an interrupted invocation gets to exit after SIGTERM.
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
}

impl Cli {
    fn from_declaration(declaration: CliDeclaration) -> Self {
        let CliDeclaration {
            driver,
            command,
            args,
            env,
        } = declaration;
        let driver: Box<dyn AgentDriver> = match driver {
            DriverKind::ClaudeCode => Box::new(ClaudeCodeDriver::new(command, args)),
            DriverKind::Codex => Box::new(CodexDriver::new(command, args)),
            DriverKind::CursorAgent => Box::new(CursorAgentDriver::new(command, args)),
            DriverKind::Generic => Box::new(GenericDriver::new(command, args)),
        };
        Self { driver, env }
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
    #[must_use]
    pub fn from_declaration(name: &str, declaration: &AgentDeclaration) -> Self {
        let clis = declaration
            .clis()
            .into_iter()
            .map(Cli::from_declaration)
            .collect();
        Self {
            name: name.to_owned(),
            declaration: declaration.clone(),
            clis,
            // Processes of one agent start their routes at different CLIs.
            turn: AtomicUsize::new(std::process::id() as usize),
        }
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
    /// while one fails.
    ///
    /// # Errors
    ///
    /// [`AgentError::Interrupted`] as soon as an attempt is interrupted;
    /// otherwise the error of the only CLI, or [`AgentError::Failed`]
    /// naming every CLI's failure when each of several failed.
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

        let status = wait(&mut child, invocation.interrupt)?;
        let join = |handle: Option<thread::JoinHandle<std::io::Result<Vec<u8>>>>| {
            handle
                .map(|h| h.join().unwrap_or_else(|_| Ok(Vec::new())))
                .transpose()
                .map(Option::unwrap_or_default)
        };
        let output = std::process::Output {
            status: status.unwrap_or_default(),
            stdout: join(stdout)?,
            stderr: join(stderr)?,
        };
        feeder.join().unwrap_or(Ok(()))?;
        {
            let mut log = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match status {
                Some(status) => writeln!(log, "=== exit: {status} ===")?,
                None => writeln!(log, "=== interrupted ===")?,
            }
        }
        if status.is_none() {
            return Err(AgentError::Interrupted);
        }
        cli.driver.interpret(&output, &request)
    }
}

/// Wait for the child; on interrupt, stop its whole process group (TERM,
/// then KILL after a grace period) and return `None`.
fn wait(child: &mut Child, interrupt: &Interrupt) -> std::io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if interrupt.is_raised() {
            signal_group(child.id(), "-TERM");
            let deadline = Instant::now() + TERMINATION_GRACE;
            while Instant::now() < deadline {
                if child.try_wait()?.is_some() {
                    return Ok(None);
                }
                thread::sleep(Duration::from_millis(100));
            }
            signal_group(child.id(), "-KILL");
            child.wait()?;
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(100));
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
        Agent::from_declaration("a", declaration.agent("a").expect("agent"))
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

    fn agent_text_all_failing() -> String {
        ROUTED
            .replace("echo '{\\\"cli\\\":1}'", "exit 3")
            .replace("echo '{\\\"cli\\\":2}'", "exit 4")
    }
}
