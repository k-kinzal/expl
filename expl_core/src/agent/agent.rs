//! [`Agent`] — the noun that realises the invocation cycle.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::agent::{
    AgentDriver, AgentError, AgentKind, AgentRequest, AgentRun, ClaudeCodeDriver, CodexDriver,
    GenericDriver,
};
use crate::declaration::AgentDeclaration;
use crate::prompt::Prompt;

/// One request to run an agent: what to say, where, and where to log.
#[derive(Debug, Clone, Copy)]
pub struct Invocation<'a> {
    /// The directory the agent works in (a node worktree).
    pub directory: &'a Path,
    /// The rendered instructions.
    pub prompt: &'a Prompt,
    /// JSON Schema the response must satisfy.
    pub response_schema: &'a serde_json::Value,
    /// Extra environment for the child (role, node).
    pub env: &'a [(String, String)],
    /// File receiving the prompt and the teed raw output.
    pub log: &'a Path,
}

/// A declared agent: a name plus the driver that speaks its CLI's dialect.
pub struct Agent {
    name: String,
    driver: Box<dyn AgentDriver>,
    env: BTreeMap<String, String>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("name", &self.name)
            .field("kind", &self.driver.kind())
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// An agent over `driver`.
    #[must_use]
    pub fn new(name: impl Into<String>, driver: Box<dyn AgentDriver>) -> Self {
        Self {
            name: name.into(),
            driver,
            env: BTreeMap::new(),
        }
    }

    /// Bring a declared agent to life: a mechanical field move into the
    /// driver named by `kind`.
    #[must_use]
    pub fn from_declaration(name: &str, declaration: &AgentDeclaration) -> Self {
        let command = declaration.command.clone();
        let args = declaration.args.clone();
        let driver: Box<dyn AgentDriver> = match declaration.kind {
            AgentKind::ClaudeCode => Box::new(ClaudeCodeDriver::new(command, args)),
            AgentKind::Codex => Box::new(CodexDriver::new(command, args)),
            AgentKind::Generic => Box::new(GenericDriver::new(command, args)),
        };
        Self {
            name: name.to_owned(),
            driver,
            env: declaration.env.clone(),
        }
    }

    /// Declared name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Driver kind.
    #[must_use]
    pub fn kind(&self) -> AgentKind {
        self.driver.kind()
    }

    /// Run the agent once and return its structured response.
    ///
    /// # Errors
    ///
    /// [`AgentError`] when the agent cannot start, fails, or replies with
    /// something that is not a JSON object.
    pub fn run(&self, invocation: &Invocation<'_>) -> Result<AgentRun, AgentError> {
        let scratch = tempfile::Builder::new().prefix("expl-agent-").tempdir()?;
        let request = AgentRequest {
            prompt: invocation.prompt,
            response_schema: invocation.response_schema,
            scratch: scratch.path(),
        };
        let mut command = self.driver.command(&request)?;
        command
            .process
            .current_dir(invocation.directory)
            .envs(&self.env)
            .envs(invocation.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut log = File::create(invocation.log)?;
        writeln!(
            log,
            "=== agent `{}` ({}) ===",
            self.name,
            self.kind().label()
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

        let status = child.wait()?;
        let join = |handle: Option<thread::JoinHandle<std::io::Result<Vec<u8>>>>| {
            handle
                .map(|h| h.join().unwrap_or_else(|_| Ok(Vec::new())))
                .transpose()
                .map(Option::unwrap_or_default)
        };
        let output = std::process::Output {
            status,
            stdout: join(stdout)?,
            stderr: join(stderr)?,
        };
        feeder.join().unwrap_or(Ok(()))?;
        {
            let mut log = log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            writeln!(log, "=== exit: {status} ===")?;
        }
        self.driver.interpret(&output, &request)
    }
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
