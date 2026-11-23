//! `expl up` and `expl down` — the declared set of processes.
//!
//! `up` is idempotent, like `docker compose up -d`: for each consumer it starts
//! only the processes missing to reach `replicas`. A record still created
//! past its startup deadline is abandoned first, so an interrupted launch
//! does not hold a slot. `down` stops every live process.

use std::path::Path;
use std::process::Command;

use crate::cli::{StopArgs, UpArgs};
use crate::dispatch::{load, process, processes};
use crate::error::CliError;
use crate::output::status;

pub(crate) fn up(start: &Path, args: &UpArgs) -> Result<(), CliError> {
    let context = load(start)?;
    let declaration = context.declaration();
    for name in &args.consumers {
        declaration.consumer(name)?;
    }
    let table = processes(context.project())?;
    // A launch that never got adopted gives its slot back; its child, if
    // it ever arrives, can no longer adopt the record.
    for record in table.abandon_unstarted()? {
        status(&format!(
            "{}: abandoned {}, never adopted",
            record.consumer, record.name
        ));
    }
    let live = table.list(false)?;
    let exe = std::env::current_exe().map_err(|e| CliError::io("locate the expl binary", e))?;
    for (name, consumer) in &declaration.consumer {
        if !args.consumers.is_empty() && !args.consumers.contains(name) {
            continue;
        }
        if !declaration.is_assigned(name) {
            status(&format!("{name}: unassigned"));
            continue;
        }
        let running = live.iter().filter(|r| &r.consumer == name).count();
        if running >= consumer.replicas {
            status(&format!("{name}: {running}/{} running", consumer.replicas));
            continue;
        }
        if context.authority().is_some() {
            table.retire_dead(name)?;
        }
        for _ in running..consumer.replicas {
            // Reuse `expl run -d` so `up` and `run -d` start processes the
            // same way; it prints the process name on stdout.
            let ok = Command::new(&exe)
                .arg("-C")
                .arg(context.project().root())
                .args(["run", name, "--detach"])
                .status()
                .map_err(|e| CliError::io("start process", e))?
                .success();
            if !ok {
                return Err(CliError::Usage(format!(
                    "could not start a process of `{name}`"
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn down(start: &Path, args: &StopArgs) -> Result<(), CliError> {
    let project = expl_core::Project::discover(start)?;
    let table = processes(&project)?;
    let live = table.list(false)?;
    if live.is_empty() {
        status("no running processes");
        return Ok(());
    }
    process::stop(&table, &live, std::time::Duration::from_secs(args.time))
}
