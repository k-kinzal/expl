//! `expl up` and `expl down` — the declared set of processes.
//!
//! `up` is idempotent, like `docker compose up -d`: for each agent it starts
//! only the processes missing to reach `replicas`. `down` stops every live
//! process.

use std::path::Path;
use std::process::Command;

use crate::cli::{StopArgs, UpArgs};
use crate::dispatch::{load, process, processes};
use crate::error::CliError;
use crate::output::status;

pub(crate) fn up(start: &Path, args: &UpArgs) -> Result<(), CliError> {
    let context = load(start)?;
    let declaration = context.declaration();
    for name in &args.agents {
        declaration.agent(name)?;
    }
    let table = processes(context.project())?;
    let live = table.list(false)?;
    let exe = std::env::current_exe().map_err(|e| CliError::io("locate the expl binary", e))?;
    for (name, agent) in &declaration.agent {
        if !args.agents.is_empty() && !args.agents.contains(name) {
            continue;
        }
        let running = live.iter().filter(|r| &r.agent == name).count();
        if running >= agent.replicas {
            status(&format!("{name}: {running}/{} running", agent.replicas));
            continue;
        }
        for _ in running..agent.replicas {
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
