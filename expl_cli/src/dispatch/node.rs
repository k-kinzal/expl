//! `expl node ...`.

use std::path::Path;

use expl_core::{Graph, NodeFilter, NodeKind, NodeStatus};
use serde_json::json;

use crate::cli::{ListingFormat, NodeAddArgs, NodeCommand, NodeLsArgs, NodeRefArgs};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{Table, clip, json_line, json_pretty, print, println, short_commit, status};
use crate::telemetry::graph_command;

pub(crate) fn node(start: &Path, command: NodeCommand) -> Result<(), CliError> {
    match command {
        NodeCommand::Ls(args) => ls(start, &args),
        NodeCommand::Inspect(args) => inspect(start, &args),
        NodeCommand::Add(args) => add(start, &args),
        NodeCommand::Retry(args) => {
            let (project, mut graph) = open(start)?;
            graph_command(&project, "node.retry", || {
                let node = graph.node(args.node)?;
                if node.kind != NodeKind::Step || node.status != NodeStatus::Failed {
                    return Err(CliError::Usage(format!(
                        "{} is a {} {}; only failed steps can be retried",
                        args.node, node.status, node.kind
                    )));
                }
                graph.replan(args.node)?;
                status(&format!("re-planned {}", args.node));
                Ok(())
            })
        }
        NodeCommand::Prune(args) => {
            let (project, mut graph) = open(start)?;
            graph_command(&project, "node.prune", || {
                let node = graph.node(args.node)?;
                if node.kind != NodeKind::Step {
                    return Err(CliError::Usage(format!(
                        "{} is a {}; retire goals with `expl goal retire`",
                        args.node, node.kind
                    )));
                }
                graph.prune(args.node, &format!("pruned: {}", args.reason))?;
                status(&format!("pruned {}", args.node));
                Ok(())
            })
        }
        NodeCommand::Logs(args) => logs(start, &args),
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
        let kind = if node.investigation {
            "investigation".to_owned()
        } else {
            node.kind.to_string()
        };
        let status = match node.answer {
            Some(answer) => format!("{} ({answer})", node.status),
            None => node.status.to_string(),
        };
        table.row(vec![
            node.id.to_string(),
            kind,
            status,
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

fn inspect(start: &Path, args: &NodeRefArgs) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    json_pretty(&describe(&graph, args.node)?)
}

fn describe(graph: &Graph, id: expl_core::NodeId) -> Result<serde_json::Value, CliError> {
    let node = graph.node(id)?;
    let lineage = graph.lineage()?;
    let ids = |nodes: Vec<expl_core::Node>| nodes.into_iter().map(|n| n.id.0).collect::<Vec<_>>();
    Ok(json!({
        "node": node,
        "predecessors": ids(graph.predecessors(id)?),
        "successors": ids(graph.successors(id)?),
        "grown_from_failure": ids(graph.derived_from(id)?),
        "explored_from": lineage.explored_from(id),
        "adopted_into": lineage.absorbed_by(id),
        "conclusions": graph.conclusions_of(id)?,
        "evaluations": graph.evaluations_of(id)?,
        "agent_runs": graph.agent_runs(Some(id), 20)?,
    }))
}

fn add(start: &Path, args: &NodeAddArgs) -> Result<(), CliError> {
    let (project, mut graph) = open(start)?;
    graph_command(&project, "node.add", || {
        let mut after = args.after.clone();
        if after.is_empty() {
            after.push(graph.origin()?.id);
        }
        let objective = args.objective.clone().unwrap_or_else(|| args.title.clone());
        let id = graph.propose(&expl_core::NewStep {
            investigation: args.investigation,
            title: &args.title,
            objective: &objective,
            after: &after,
            before: &args.before,
            derived_from: args.derived_from,
            proposed_by: None,
        })?;
        if let Some(priority) = args.priority {
            graph.plan_step(id, priority)?;
        }
        status(&format!("proposed {id}"));
        println(&id.0.to_string())
    })
}

/// The newest agent log about a node.
fn logs(start: &Path, args: &NodeRefArgs) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    graph.node(args.node)?;
    let runs = graph.agent_runs(Some(args.node), 1)?;
    let Some(run) = runs.first() else {
        status(&format!("no agent runs for {}", args.node));
        return Ok(());
    };
    let Some(log_path) = &run.log_path else {
        return Err(CliError::Usage(format!(
            "agent run {} has no log",
            run.id.0
        )));
    };
    status(&format!(
        "agent run {} ({} agent `{}`, {}): {log_path}",
        run.id.0, run.role, run.agent, run.status
    ));
    let text = std::fs::read_to_string(log_path)
        .map_err(|e| CliError::io(format!("read {log_path}"), e))?;
    print(&text)
}
