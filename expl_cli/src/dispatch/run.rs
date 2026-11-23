//! `expl run <CONSUMER>` — start a worker for an event consumer.
//!
//! In the foreground the process's activity goes to stderr and its log
//! file; Ctrl-C (or `expl stop`) stops it gracefully: the current agent
//! invocation is interrupted and its event is released for redelivery.
//!
//! With `-d` the record is created here, and a child `expl` adopts it in
//! its own process group with stdout/stderr going to the log file — the
//! way `docker run -d` hands a container to the daemon. The child must
//! adopt it within its startup deadline; a record left unadopted is
//! abandoned rather than holding its replica slot forever.

use std::fs::{File, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use expl_core::{Interrupt, ProcessId, ProcessOptions, ProcessRecord, Project};
use expl_runtime::worker::Worker;

use crate::cli::RunArgs;
use crate::dispatch::{load, processes};
use crate::error::CliError;
use crate::observer::ActivityLog;
use crate::output::{println, status};
use crate::telemetry::Telemetry;

pub(crate) fn run(start: &Path, args: &RunArgs) -> Result<(), CliError> {
    let context = load(start)?;
    let consumer = context.consumer(&args.consumer)?;
    let project = context.project();
    let mut table = processes(project)?;
    std::fs::create_dir_all(project.processes_dir())
        .map_err(|e| CliError::io("create process log directory", e))?;

    let options = ProcessOptions {
        once: args.once,
        exit_when_idle: args.exit_when_idle,
    };
    if args.detach {
        let (record, spawned) = if let Some(authority) = context.authority() {
            // Under a daemon the worker is the daemon's: it is started
            // from what the project registers, and restarted after an
            // election.
            let explorer = crate::daemon::register(project, authority.cluster())?;
            let exe =
                std::env::current_exe().map_err(|e| CliError::io("locate the expl binary", e))?;
            let record = table.create_resumable(consumer.name(), args.name.as_deref(), options)?;
            let spawned =
                expl_runtime::daemon::spawn_worker(&exe, authority, &explorer, &record, options)
                    .map(drop)
                    .map_err(|e| CliError::io("start detached process", e));
            (record, spawned)
        } else {
            let record = table.create(consumer.name(), args.name.as_deref(), true)?;
            let spawned = spawn_detached(project, &record, options).map(drop);
            (record, spawned)
        };
        if let Err(error) = spawned {
            table.exit(record.id, &format!("failed to start: {error}"))?;
            return Err(error);
        }
        status(&format!("started {} ({})", record.name, record.id));
        return println(&record.name);
    }

    let (record, tee) = if let Some(id) = args.adopt {
        let record = table.get(ProcessId(id))?;
        if record.consumer != consumer.name() {
            return Err(CliError::Usage(format!(
                "process {} belongs to consumer `{}`",
                record.id, record.consumer
            )));
        }
        // Detached: stderr already is the log file.
        (record, None)
    } else {
        let record = table.create(consumer.name(), args.name.as_deref(), false)?;
        let log = open_log(&record)?;
        (record, Some(log))
    };
    let interrupt =
        Interrupt::on_signals().map_err(|e| CliError::io("install signal handlers", e))?;
    let observer = ActivityLog::new(&record.name, tee);
    let _telemetry = Telemetry::start(project, context.declaration().telemetry.as_ref(), true);
    // The worker is the runtime's. What it executes for each event is this
    // binary again, making one reaction (`expl react`).
    let exe = std::env::current_exe().map_err(|e| CliError::io("locate the expl binary", e))?;
    let registration = context.declaration().registration_for(&exe, project.root());
    Worker {
        directory: project.root(),
        database: &project.database_path(),
        state: &project.state_dir(),
        logs: &project.processes_dir(),
        registration: &registration,
        consumer: consumer.name(),
        id: record.id,
        interrupt,
        authority: context.authority(),
    }
    .run(options, &observer)?;
    Ok(())
}

fn open_log(record: &ProcessRecord) -> Result<File, CliError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&record.log_path)
        .map_err(|e| CliError::io(format!("open {}", record.log_path), e))
}

fn spawn_detached(
    project: &Project,
    record: &ProcessRecord,
    options: ProcessOptions,
) -> Result<std::process::Child, CliError> {
    let exe = std::env::current_exe().map_err(|e| CliError::io("locate the expl binary", e))?;
    let log = open_log(record)?;
    let log_err = log
        .try_clone()
        .map_err(|e| CliError::io(format!("open {}", record.log_path), e))?;
    let mut command = Command::new(exe);
    command
        .arg("-C")
        .arg(project.root())
        .args(["run", &record.consumer, "--adopt", &record.id.0.to_string()])
        .current_dir(project.root())
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .process_group(0);
    if options.once {
        command.arg("--once");
    }
    if options.exit_when_idle {
        command.arg("--exit-when-idle");
    }
    // The child outlives this command; it is not waited for.
    command
        .spawn()
        .map_err(|e| CliError::io("start detached process", e))
}
