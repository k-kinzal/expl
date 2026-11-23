//! `expl event ls` — the event log, optionally as one consumer sees it.

use std::path::Path;

use expl_core::{EventId, Inbox};
use serde_json::json;

use crate::cli::{EventLsArgs, ListingFormat};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{Table, json_line, print, println};

pub(crate) fn ls(start: &Path, args: &EventLsArgs) -> Result<(), CliError> {
    let (project, graph) = open(start)?;
    // Oldest first: from `--after` forward, or else the newest `--limit`.
    let mut events = graph.events(args.after.map(EventId), i64::MAX as usize)?;
    if !args.kind.is_empty() {
        events.retain(|e| args.kind.contains(&e.kind));
    }
    if args.after.is_some() {
        events.truncate(args.limit);
    } else {
        events.drain(..events.len().saturating_sub(args.limit));
    }
    let inbox = args
        .consumer
        .as_deref()
        .map(|consumer| Inbox::open(&project.database_path(), consumer))
        .transpose()?;
    let delivery = |id: EventId| -> Result<String, CliError> {
        Ok(match &inbox {
            Some(inbox) => inbox.status_of(id)?.unwrap_or_else(|| "undelivered".into()),
            None => String::new(),
        })
    };
    if args.listing.quiet {
        for event in &events {
            println(&event.id.0.to_string())?;
        }
        return Ok(());
    }
    if args.listing.format == ListingFormat::Json {
        for event in &events {
            let mut value = json!(event);
            if inbox.is_some() {
                value["delivery"] = json!(delivery(event.id)?);
            }
            json_line(&value)?;
        }
        return Ok(());
    }
    let mut header = vec!["ID", "KIND", "NODE", "CREATED"];
    if inbox.is_some() {
        header.push("DELIVERY");
    }
    let mut table = Table::new(&header);
    for event in &events {
        let mut row = vec![
            event.id.to_string(),
            event.kind.to_string(),
            event.node.map_or_else(|| "-".to_owned(), |n| n.to_string()),
            event.created_at.clone(),
        ];
        if inbox.is_some() {
            row.push(delivery(event.id)?);
        }
        table.row(row);
    }
    print(&table.render())
}
