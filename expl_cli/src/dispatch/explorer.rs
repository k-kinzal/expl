//! `expl explorer ...` — the explorers a daemon hosts.
//!
//! These run on the leader, for any explorer: nothing here knows what an
//! explorer's events mean. A project made with `expl init` is one explorer
//! among them, and is usually managed with the project commands instead.

use std::path::PathBuf;
use std::time::Duration;

use expl_runtime::cluster::Authority;
use expl_runtime::daemon::{self, DaemonError, Explorer, Launch, Registry};
use expl_runtime::event::EventLog;
use expl_runtime::store;

use crate::cli::{ExplorerCommand, ListingArgs, ListingFormat};
use crate::dispatch::process;
use crate::error::CliError;
use crate::output::{Table, json_line, print, println, status};

pub(crate) fn explorer(authority: &Authority, command: ExplorerCommand) -> Result<(), CliError> {
    let cluster = authority.cluster();
    let registry = Registry::of(cluster);
    match command {
        ExplorerCommand::Ls(args) => ls(&registry, &args),
        ExplorerCommand::Add(args) => {
            // The path was typed where the command was, not where it runs.
            let file = match daemon::origin() {
                Some(origin) => origin.join(&args.file),
                None => args.file,
            };
            let explorer = Explorer::from_file(&file)?;
            explorer.prepare(Some(cluster))?;
            registry.join(&explorer)?;
            status(&format!(
                "registered {} at {}",
                explorer.name,
                explorer.directory.display()
            ));
            println(&explorer.name)
        }
        ExplorerCommand::Rm(args) => {
            let explorer = registry.leave(&args.name)?;
            status(&format!("removed {}", explorer.name));
            println(&explorer.name)
        }
        ExplorerCommand::Up(args) => {
            let explorer = registry.expect(&args.name)?;
            started(daemon::up(&exe()?, authority, &explorer, &args.consumers)?)
        }
        ExplorerCommand::Down(args) => {
            let table = registry.expect(&args.name)?.processes()?;
            let live = table.list(false)?;
            if live.is_empty() {
                status("no running processes");
                return Ok(());
            }
            process::stop(&table, &live, Duration::from_secs(args.stop.time))
        }
        ExplorerCommand::Ps(args) => {
            process::ls(&registry.expect(&args.name)?.processes()?, &args.ps)
        }
        ExplorerCommand::Publish(args) => {
            let explorer = registry.expect(&args.name)?;
            let conn = store::open(&explorer.database).map_err(DaemonError::from)?;
            let event = match &args.payload {
                Some(payload) => {
                    let payload: serde_json::Value =
                        serde_json::from_str(payload).map_err(|error| {
                            CliError::Usage(format!("--payload is not JSON: {error}"))
                        })?;
                    EventLog::publish_with(&conn, &args.kind, args.subject, &payload)
                        .map_err(|error| CliError::Usage(error.to_string()))?
                }
                None => EventLog::publish(&conn, &args.kind, args.subject)?,
            };
            println(&event.0.to_string())
        }
    }
}

fn exe() -> Result<PathBuf, CliError> {
    std::env::current_exe().map_err(|e| CliError::io("locate the expl binary", e))
}

/// Say what `up` did, the way `expl up` always has: the names of the
/// workers started go to stdout.
pub(crate) fn started(launches: Vec<Launch>) -> Result<(), CliError> {
    for launch in launches {
        match launch {
            Launch::Abandoned(record) => status(&format!(
                "{}: abandoned {}, never adopted",
                record.consumer, record.name
            )),
            Launch::Unassigned(consumer) => status(&format!("{consumer}: unassigned")),
            Launch::Running {
                consumer,
                running,
                replicas,
            } => status(&format!("{consumer}: {running}/{replicas} running")),
            Launch::Started(record) => {
                status(&format!("started {} ({})", record.name, record.id));
                println(&record.name)?;
            }
        }
    }
    Ok(())
}

fn ls(registry: &Registry, args: &ListingArgs) -> Result<(), CliError> {
    let explorers = registry.explorers()?;
    if args.quiet {
        for explorer in &explorers {
            println(&explorer.name)?;
        }
        return Ok(());
    }
    if args.format == ListingFormat::Json {
        for explorer in &explorers {
            json_line(explorer)?;
        }
        return Ok(());
    }
    let mut out = Table::new(&["NAME", "POOL", "CONSUMERS", "WORKERS", "DIRECTORY"]);
    for explorer in &explorers {
        // A database that cannot be read is shown, not fatal to the list.
        let workers = explorer
            .processes()
            .ok()
            .and_then(|table| table.list(false).ok())
            .map_or_else(|| "?".to_owned(), |live| live.len().to_string());
        out.row(vec![
            explorer.name.clone(),
            explorer.pool().to_owned(),
            explorer.registration.consumer.len().to_string(),
            workers,
            explorer.directory.display().to_string(),
        ]);
    }
    print(&out.render())
}
