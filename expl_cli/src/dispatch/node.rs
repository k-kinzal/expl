//! `expl node ...` and `expl inspect`.

use std::path::Path;

use expl_core::{Graph, NodeFilter, NodeKind, NodeStatus};
use serde_json::json;

use crate::cli::{InspectArgs, ListingFormat, NodeAddArgs, NodeCommand, NodeLsArgs};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{Table, clip, json_line, json_pretty, print, println, short_commit, status};

pub(crate) fn node(start: &Path, command: NodeCommand) -> Result<(), CliError> {
    match command {
        NodeCommand::Ls(args) => ls(start, &args),
        NodeCommand::Inspect(args) => inspect(start, &args),
        NodeCommand::Add(args) => add(start, &args),
        NodeCommand::Retry(args) => {
            let (_, graph) = open(start)?;
            let node = graph.node(args.node)?;
            if node.kind != NodeKind::Step || node.status != NodeStatus::Failed {
                return Err(CliError::Usage(format!(
                    "{} is a {} {}; only failed steps can be retried",
                    args.node, node.status, node.kind
                )));
            }
            graph.set_status(args.node, NodeStatus::Planned)?;
            status(&format!("re-planned {}", args.node));
            Ok(())
        }
        NodeCommand::Prune(args) => {
            let (_, graph) = open(start)?;
            let node = graph.node(args.node)?;
            let prunable = node.kind == NodeKind::Step
                && matches!(
                    node.status,
                    NodeStatus::Proposed | NodeStatus::Planned | NodeStatus::Failed
                );
            if !prunable {
                return Err(CliError::Usage(format!(
                    "{} is a {} {}; only proposed, planned, or failed steps can be pruned",
                    args.node, node.status, node.kind
                )));
            }
            graph.conclude(
                args.node,
                NodeStatus::Pruned,
                &format!("pruned: {}", args.reason),
            )?;
            status(&format!("pruned {}", args.node));
            Ok(())
        }
    }
}

fn ls(start: &Path, args: &NodeLsArgs) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    let nodes = graph.nodes(&NodeFilter {
        kinds: args.kind.clone(),
        statuses: args.status.clone(),
    })?;
    if args.listing.quiet {
        for node in &nodes {
            println(&node.id.0.to_string())?;
        }
        return Ok(());
    }
    if args.listing.format == ListingFormat::Json {
        for node in &nodes {
            json_line(node)?;
        }
        return Ok(());
    }
    let mut table = Table::new(&["ID", "KIND", "STATUS", "PRIO", "COMMIT", "TITLE"]);
    for node in &nodes {
        table.row(vec![
            node.id.to_string(),
            node.kind.to_string(),
            node.status.to_string(),
            if node.kind == NodeKind::Step {
                node.priority.to_string()
            } else {
                "-".into()
            },
            short_commit(node.head_commit.as_deref()),
            clip(&node.title, 70),
        ]);
    }
    print(&table.render())
}

pub(crate) fn inspect(start: &Path, args: &InspectArgs) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    json_pretty(&describe(&graph, args.node)?)
}

fn describe(graph: &Graph, id: expl_core::NodeId) -> Result<serde_json::Value, CliError> {
    let node = graph.node(id)?;
    let ids = |nodes: Vec<expl_core::Node>| nodes.into_iter().map(|n| n.id.0).collect::<Vec<_>>();
    Ok(json!({
        "node": node,
        "predecessors": ids(graph.predecessors(id)?),
        "successors": ids(graph.successors(id)?),
        "evaluations": graph.evaluations_of(id)?,
        "agent_runs": graph.agent_runs(Some(id), 20)?,
    }))
}

fn add(start: &Path, args: &NodeAddArgs) -> Result<(), CliError> {
    let (_, mut graph) = open(start)?;
    let mut after = args.after.clone();
    if after.is_empty() {
        after.push(graph.origin()?.id);
    }
    let objective = args.objective.clone().unwrap_or_else(|| args.title.clone());
    let id = graph.propose_step(&args.title, &objective, &after, &args.before)?;
    if let Some(priority) = args.priority {
        graph.plan_step(id, priority)?;
    }
    status(&format!("proposed {id}"));
    println(&id.0.to_string())
}
