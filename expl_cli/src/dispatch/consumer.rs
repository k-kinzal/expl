//! `expl consumer ls` — event subscriptions, workers, and backlog.

use std::path::Path;

use expl_core::Inbox;
use serde_json::json;

use crate::cli::{ListingArgs, ListingFormat};
use crate::dispatch::{load, processes};
use crate::error::CliError;
use crate::output::{Table, json_line, print, println};

pub(crate) fn ls(start: &Path, listing: &ListingArgs) -> Result<(), CliError> {
    let context = load(start)?;
    let declaration = context.declaration();
    let database = context.project().database_path();
    let live = processes(context.project())?.list(false)?;
    let mut table = Table::new(&[
        "NAME",
        "HANDLER",
        "GROUP",
        "DRIVER",
        "REPLICAS",
        "CONCURRENCY",
        "RUNNING",
        "BACKLOG",
        "ON",
    ]);
    for (name, consumer) in &declaration.consumer {
        if listing.quiet {
            println(name)?;
            continue;
        }
        let backlog = Inbox::open(&database, name)?.declared_backlog(declaration)?;
        let running = live.iter().filter(|r| &r.consumer == name).count();
        let kinds: Vec<&str> = consumer.on.iter().map(|k| k.as_str()).collect();
        let drivers: Vec<_> = consumer.clis().iter().map(|cli| cli.driver).collect();
        // Daemon agents are shown by the names the consumer uses, then its
        // own CLIs by their drivers.
        let labels: Vec<String> = consumer
            .agents
            .iter()
            .map(|agent| format!("@{agent}"))
            .chain(drivers.iter().map(|d| d.label().to_owned()))
            .collect();
        if listing.format == ListingFormat::Json {
            json_line(&json!({
                "name": name,
                "handler": consumer.handler,
                "group": consumer.group,
                "on": kinds,
                "role": consumer.handler,
                "driver": consumer.driver,
                "drivers": drivers,
                "agents": consumer.agents,
                "replicas": consumer.replicas,
                "concurrency": consumer.concurrency,
                "running": running,
                "backlog": backlog,
                "reacts_to": kinds,
            }))?;
            continue;
        }
        table.row(vec![
            name.clone(),
            consumer.handler.to_string(),
            consumer.group.clone().unwrap_or_else(|| "-".to_owned()),
            labels.join("|"),
            consumer.replicas.to_string(),
            consumer
                .concurrency
                .map_or_else(|| "-".to_owned(), |limit| limit.to_string()),
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
