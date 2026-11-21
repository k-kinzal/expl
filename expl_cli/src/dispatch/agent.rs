//! `expl agent ls` — declared agents, their processes, and their backlog.

use std::path::Path;

use expl_core::Inbox;
use serde_json::json;

use crate::cli::{ListingArgs, ListingFormat};
use crate::dispatch::{load, processes};
use crate::error::CliError;
use crate::output::{Table, json_line, print, println};

pub(crate) fn ls(start: &Path, listing: &ListingArgs) -> Result<(), CliError> {
    let context = load(start)?;
    let database = context.project().database_path();
    let live = processes(context.project())?.list(false)?;
    let mut table = Table::new(&[
        "NAME",
        "ROLE",
        "DRIVER",
        "REPLICAS",
        "RUNNING",
        "BACKLOG",
        "REACTS TO",
    ]);
    for (name, agent) in &context.declaration().agent {
        if listing.quiet {
            println(name)?;
            continue;
        }
        let reactions = agent.role.reactions();
        let backlog = Inbox::open(&database, name)?.backlog(reactions)?;
        let running = live.iter().filter(|r| &r.agent == name).count();
        let kinds: Vec<&str> = reactions.iter().map(|k| k.as_str()).collect();
        let drivers: Vec<_> = agent.clis().iter().map(|cli| cli.driver).collect();
        let labels: Vec<&str> = drivers.iter().map(|d| d.label()).collect();
        if listing.format == ListingFormat::Json {
            json_line(&json!({
                "name": name,
                "role": agent.role,
                "driver": agent.driver,
                "drivers": drivers,
                "replicas": agent.replicas,
                "running": running,
                "backlog": backlog,
                "reacts_to": kinds,
            }))?;
            continue;
        }
        table.row(vec![
            name.clone(),
            agent.role.to_string(),
            labels.join("|"),
            agent.replicas.to_string(),
            running.to_string(),
            backlog.to_string(),
            kinds.join(", "),
        ]);
    }
    if listing.quiet || listing.format == ListingFormat::Json {
        return Ok(());
    }
    print(&table.render())
}
