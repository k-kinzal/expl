//! `expl path ls`.

use std::path::Path;

use crate::cli::{ListingArgs, ListingFormat};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{Table, clip, json_line, print, println, status};

pub(crate) fn ls(start: &Path, listing: &ListingArgs) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    let Some(plan) = graph.latest_plan()? else {
        status("no plan yet (run `expl plan` or `expl run`)");
        return Ok(());
    };
    if listing.quiet {
        for path in &plan.paths {
            let nodes: Vec<String> = path.nodes.iter().map(|n| n.0.to_string()).collect();
            println(&nodes.join(" "))?;
        }
        return Ok(());
    }
    if listing.format == ListingFormat::Json {
        for path in &plan.paths {
            json_line(path)?;
        }
        return Ok(());
    }
    status(&format!("plan {}: {}", plan.id.0, clip(&plan.summary, 200)));
    let mut table = Table::new(&["GOAL", "PATH", "RATIONALE"]);
    for path in &plan.paths {
        // Subgoals the path passes through show as `[id]`.
        let nodes: Vec<String> = path
            .nodes
            .iter()
            .map(|id| match graph.node(*id) {
                Ok(node) if node.kind == expl_core::NodeKind::Goal => format!("[{}]", id.0),
                _ => id.to_string(),
            })
            .collect();
        table.row(vec![
            path.goal.to_string(),
            nodes.join(" → "),
            clip(&path.rationale, 80),
        ]);
    }
    print(&table.render())
}
