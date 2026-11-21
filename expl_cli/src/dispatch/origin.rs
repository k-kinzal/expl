//! `expl origin ls` — the trunk's lineage. The trunk itself moves with
//! git; running processes follow it.

use std::collections::BTreeMap;
use std::path::Path;

use expl_core::{Evaluation, Graph, NodeId};
use serde_json::json;

use crate::cli::{ListingArgs, ListingFormat};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{Table, clip, json_line, print, println, short_commit};

pub(crate) fn ls(start: &Path, listing: &ListingArgs) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    let lineage = graph.lineage()?;
    let verdicts = latest_verdicts(&graph)?;
    if listing.quiet {
        for epoch in &lineage.epochs {
            println(&epoch.origin.0.to_string())?;
        }
        return Ok(());
    }
    let mut table = Table::new(&[
        "ID", "STATUS", "COMMIT", "BRANCH", "ADOPTED", "SCORES", "WHEN", "TITLE",
    ]);
    for epoch in &lineage.epochs {
        let node = graph.node(epoch.origin)?;
        let judged = verdicts.get(&epoch.origin).cloned().unwrap_or_default();
        if listing.format == ListingFormat::Json {
            json_line(&json!({
                "origin": node,
                "previous": epoch.previous,
                "adopted": epoch.adopted,
                "absorbed": epoch.absorbed,
                "verdicts": judged.iter().map(|e| json!({
                    "goal": e.goal,
                    "score": e.score,
                    "satisfied": e.satisfied,
                })).collect::<Vec<_>>(),
            }))?;
            continue;
        }
        table.row(vec![
            node.id.to_string(),
            if lineage.current == Some(node.id) {
                "current".to_owned()
            } else {
                node.status.to_string()
            },
            short_commit(node.head_commit.as_deref()),
            node.branch.clone().unwrap_or_else(|| "-".into()),
            ids(&epoch.adopted),
            if judged.is_empty() {
                "-".to_owned()
            } else {
                judged
                    .iter()
                    .map(|e| {
                        format!(
                            "{}:{}{}",
                            e.goal,
                            e.score,
                            if e.satisfied { "✓" } else { "" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            when(&node.created_at),
            clip(&node.title, 50),
        ]);
    }
    if listing.format == ListingFormat::Table {
        print(&table.render())?;
    }
    Ok(())
}

/// Each node's latest verdict per goal.
fn latest_verdicts(graph: &Graph) -> Result<BTreeMap<NodeId, Vec<Evaluation>>, CliError> {
    let mut latest: BTreeMap<(NodeId, NodeId), Evaluation> = BTreeMap::new();
    for evaluation in graph.evaluations()? {
        latest.insert((evaluation.node, evaluation.goal), evaluation);
    }
    let mut by_node: BTreeMap<NodeId, Vec<Evaluation>> = BTreeMap::new();
    for ((node, _), evaluation) in latest {
        by_node.entry(node).or_default().push(evaluation);
    }
    Ok(by_node)
}

fn ids(ids: &[NodeId]) -> String {
    if ids.is_empty() {
        "-".to_owned()
    } else {
        ids.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// `2026-11-21T09:30:12.345Z` → `2026-11-21 09:30`.
fn when(timestamp: &str) -> String {
    timestamp
        .chars()
        .take(16)
        .collect::<String>()
        .replace('T', " ")
}
