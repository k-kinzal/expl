//! `expl hypothesis ls` — the hypotheses a human chooses from: change steps to
//! merge, and the findings of investigations that answer a goal.

use std::path::Path;

use serde_json::json;

use crate::cli::{ListingArgs, ListingFormat};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{Table, clip, json_line, print, println};

pub(crate) fn ls(start: &Path, listing: &ListingArgs) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    let satisfying = graph.satisfying()?;
    let lineage = graph.lineage()?;
    if listing.quiet {
        for satisfied in &satisfying {
            println(&satisfied.node.0.to_string())?;
        }
        return Ok(());
    }
    let mut table = Table::new(&[
        "NODE",
        "GOAL",
        "SCORE",
        "FROM",
        "ADOPTED",
        "BRANCH/ANSWER",
        "TITLE",
        "REASON",
    ]);
    for satisfied in &satisfying {
        let node = graph.node(satisfied.node)?;
        let branch = node.branch.clone().unwrap_or_default();
        let shown = match node.answer {
            Some(answer) => format!("answer: {answer}"),
            None => branch.clone(),
        };
        let from = lineage.explored_from(satisfied.node);
        let behind = from.and_then(|o| lineage.behind(o)).unwrap_or(0);
        let adopted = lineage.absorbed_by(satisfied.node);
        if listing.format == ListingFormat::Json {
            json_line(&json!({
                "node": satisfied.node,
                "goal": satisfied.goal,
                "score": satisfied.score,
                "explored_from": from,
                "behind": behind,
                "adopted_into": adopted,
                "branch": branch,
                "investigation": node.investigation,
                "answer": node.answer,
                "head_commit": node.head_commit,
                "title": node.title,
                "reason": satisfied.reason,
            }))?;
            continue;
        }
        table.row(vec![
            satisfied.node.to_string(),
            satisfied.goal.to_string(),
            satisfied.score.to_string(),
            match (from, behind) {
                (Some(origin), 0) => origin.to_string(),
                (Some(origin), n) => format!("{origin} ({n} behind)"),
                (None, _) => "-".to_owned(),
            },
            adopted.map_or_else(|| "-".to_owned(), |o| o.to_string()),
            shown,
            clip(&node.title, 40),
            clip(&satisfied.reason, 60),
        ]);
    }
    if listing.format == ListingFormat::Table {
        print(&table.render())?;
    }
    Ok(())
}
