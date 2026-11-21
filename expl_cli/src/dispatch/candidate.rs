//! `expl candidate ls` — the steps a human chooses from.

use std::path::Path;

use serde_json::json;

use crate::cli::{ListingArgs, ListingFormat};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{Table, clip, json_line, print, println};

pub(crate) fn ls(start: &Path, listing: &ListingArgs) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    let candidates = graph.candidates()?;
    if listing.quiet {
        for candidate in &candidates {
            println(&candidate.node.0.to_string())?;
        }
        return Ok(());
    }
    let mut table = Table::new(&["NODE", "GOAL", "SCORE", "BRANCH", "TITLE", "REASON"]);
    for candidate in &candidates {
        let node = graph.node(candidate.node)?;
        let branch = node.branch.clone().unwrap_or_default();
        if listing.format == ListingFormat::Json {
            json_line(&json!({
                "node": candidate.node,
                "goal": candidate.goal,
                "score": candidate.score,
                "branch": branch,
                "head_commit": node.head_commit,
                "title": node.title,
                "reason": candidate.reason,
            }))?;
            continue;
        }
        table.row(vec![
            candidate.node.to_string(),
            candidate.goal.to_string(),
            candidate.score.to_string(),
            branch,
            clip(&node.title, 40),
            clip(&candidate.reason, 60),
        ]);
    }
    if listing.format == ListingFormat::Table {
        print(&table.render())?;
    }
    Ok(())
}
