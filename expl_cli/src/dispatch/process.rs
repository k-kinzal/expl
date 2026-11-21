//! `expl process ...` and its aliases (`ps`, `logs`, `stop`, `kill`, `rm`).

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use expl_core::{ProcessRecord, ProcessState, Processes, Project};

use crate::cli::{ListingFormat, ProcessCommand, PsArgs};
use crate::dispatch::{logs, processes};
use crate::error::CliError;
use crate::output::{Table, clip, json_line, json_pretty, print, println, status};

pub(crate) fn process(start: &Path, command: ProcessCommand) -> Result<(), CliError> {
    let project = Project::discover(start)?;
    let table = processes(&project)?;
    match command {
        ProcessCommand::Ls(args) => ls(&table, &args),
        ProcessCommand::Inspect(args) => json_pretty(&resolve(&table, &args.process)?),
        ProcessCommand::Logs(args) => {
            let record = resolve(&table, &args.process)?;
            logs::print_log(&table, &record, args.follow)
        }
        ProcessCommand::Stop(args) => {
            let records = resolve_all(&table, &args.processes)?;
            stop(&table, &records, Duration::from_secs(args.stop.time))
        }
        ProcessCommand::Kill(args) => {
            for record in resolve_all(&table, &args.processes)? {
                if record.is_live() {
                    kill(&table, &record)?;
                    status(&format!("killed {}", record.name));
                } else {
                    status(&format!("{} is not running", record.name));
                }
            }
            Ok(())
        }
        ProcessCommand::Rm(args) => {
            let records = resolve_all(&table, &args.processes)?;
            if !args.force
                && let Some(live) = records.iter().find(|r| r.is_live())
            {
                return Err(CliError::Usage(format!(
                    "{} is {}; stop it first or use --force",
                    live.name, live.state
                )));
            }
            for record in records {
                if record.is_live() {
                    kill(&table, &record)?;
                }
                table.remove(record.id)?;
                std::fs::remove_file(&record.log_path).ok();
                println(&record.name)?;
            }
            Ok(())
        }
    }
}

fn ls(table: &Processes, args: &PsArgs) -> Result<(), CliError> {
    let records = table.list(args.all)?;
    if args.listing.quiet {
        for record in &records {
            println(&record.id.to_string())?;
        }
        return Ok(());
    }
    if args.listing.format == ListingFormat::Json {
        for record in &records {
            json_line(record)?;
        }
        return Ok(());
    }
    let mut out = Table::new(&["ID", "NAME", "AGENT", "STATE", "PID", "CREATED", "STATUS"]);
    for record in &records {
        out.row(vec![
            record.id.to_string(),
            record.name.clone(),
            record.agent.clone(),
            record.state.to_string(),
            record.pid.map_or_else(|| "-".to_owned(), |p| p.to_string()),
            record.created_at.clone(),
            clip(&describe(record), 80),
        ]);
    }
    print(&out.render())
}

/// One line about what a process is doing or how it ended.
pub(crate) fn describe(record: &ProcessRecord) -> String {
    match record.state {
        ProcessState::Created => "starting".to_owned(),
        ProcessState::Running if record.stop_requested => "stopping".to_owned(),
        ProcessState::Running => record.activity.clone().unwrap_or_else(|| "running".into()),
        ProcessState::Dead => "no heartbeat (crashed or killed)".to_owned(),
        ProcessState::Exited => record.exit_reason.clone().unwrap_or_default(),
    }
}

pub(crate) fn resolve(table: &Processes, target: &str) -> Result<ProcessRecord, CliError> {
    table
        .find(target)?
        .ok_or_else(|| CliError::Usage(format!("no process `{target}`")))
}

fn resolve_all(table: &Processes, targets: &[String]) -> Result<Vec<ProcessRecord>, CliError> {
    targets.iter().map(|t| resolve(table, t)).collect()
}

/// Ask every live process in `records` to stop, wait up to `grace` for
/// them to exit, and kill the rest.
pub(crate) fn stop(
    table: &Processes,
    records: &[ProcessRecord],
    grace: Duration,
) -> Result<(), CliError> {
    let mut waiting = Vec::new();
    for record in records {
        if !record.is_live() {
            status(&format!("{} is not running", record.name));
            continue;
        }
        table.request_stop(record.id)?;
        if let Some(pid) = record.pid {
            signal(pid, "-TERM");
        }
        waiting.push(record.clone());
    }
    let deadline = Instant::now() + grace;
    while !waiting.is_empty() {
        let mut still = Vec::new();
        for record in waiting {
            let current = table.get(record.id)?;
            if current.is_live() {
                still.push(current);
            } else {
                status(&format!("stopped {}", current.name));
                println(&current.name)?;
            }
        }
        waiting = still;
        if waiting.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            for record in &waiting {
                kill(table, record)?;
                status(&format!(
                    "killed {} after {}s",
                    record.name,
                    grace.as_secs()
                ));
                println(&record.name)?;
            }
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// SIGKILL the process and record it as exited.
fn kill(table: &Processes, record: &ProcessRecord) -> Result<(), CliError> {
    if let Some(pid) = record.pid {
        signal(pid, "-KILL");
    }
    table.exit(record.id, "killed")?;
    Ok(())
}

fn signal(pid: u32, signal: &str) {
    // Best effort: the process may already be gone.
    Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok();
}
