//! `expl graph show`.

use std::fmt::Write;
use std::path::Path;

use expl_core::{Node, NodeFilter, NodeKind, NodeStatus};
use serde_json::json;

use crate::cli::{GraphFormat, GraphShowArgs};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{clip, json_pretty, print, short_commit};

pub(crate) fn show(start: &Path, args: &GraphShowArgs) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    let nodes = graph.nodes(&NodeFilter::default())?;
    let edges = graph.edges()?;
    match args.format {
        GraphFormat::Json => json_pretty(&json!({ "nodes": nodes, "edges": edges })),
        GraphFormat::Mermaid => print(&mermaid(&nodes, &edges)),
        GraphFormat::Text => {
            let mut satisfying = std::collections::BTreeMap::new();
            for evaluation in graph.satisfying()? {
                satisfying
                    .entry(evaluation.node)
                    .or_insert_with(Vec::new)
                    .push(evaluation.goal.to_string());
            }
            let mut out = String::new();
            for node in &nodes {
                let after: Vec<String> = edges
                    .iter()
                    .filter(|e| e.target == node.id)
                    .map(|e| e.source.to_string())
                    .collect();
                let _ = write!(
                    out,
                    "{:<5} {:<6} {:<9} {:<7} {}",
                    node.id.to_string(),
                    node.kind.as_str(),
                    node.status.as_str(),
                    short_commit(node.head_commit.as_deref()),
                    clip(&node.title, 60)
                );
                if !after.is_empty() {
                    let _ = write!(out, "  ← {}", after.join(", "));
                }
                if let Some(goals) = satisfying.get(&node.id) {
                    let _ = write!(out, "  ★ satisfies {}", goals.join(", "));
                }
                out.push('\n');
            }
            print(&out)
        }
    }
}

fn mermaid(nodes: &[Node], edges: &[expl_core::Edge]) -> String {
    let mut out = String::from("flowchart TD\n");
    for node in nodes {
        let label = format!("{} {}", node.id, clip(&node.title, 40)).replace('"', "'");
        let shape = match node.kind {
            NodeKind::Origin => format!("n{}([\"{label}\"])", node.id.0),
            NodeKind::Goal => format!("n{}{{{{\"{label}\"}}}}", node.id.0),
            NodeKind::Step => format!("n{}[\"{label}\"]", node.id.0),
        };
        let _ = writeln!(out, "  {shape}:::{}", node.status.as_str());
    }
    for edge in edges {
        let _ = writeln!(out, "  n{} --> n{}", edge.source.0, edge.target.0);
    }
    for (status, style) in [
        (NodeStatus::Proposed, "stroke-dasharray: 4 4"),
        (NodeStatus::Planned, "stroke:#888"),
        (NodeStatus::Running, "stroke:#d90,stroke-width:3px"),
        (NodeStatus::Completed, "stroke:#2a2,stroke-width:2px"),
        (NodeStatus::Failed, "stroke:#c22,stroke-width:2px"),
        (NodeStatus::Pruned, "opacity:0.4"),
        (NodeStatus::Active, "stroke:#26c,stroke-width:3px"),
        (NodeStatus::Superseded, "opacity:0.6"),
    ] {
        let _ = writeln!(out, "  classDef {} {style}", status.as_str());
    }
    out
}
