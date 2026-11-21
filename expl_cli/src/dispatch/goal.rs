//! `expl goal ...`.

use std::path::Path;

use expl_core::{NodeKind, NodeStatus};

use crate::cli::{GoalCommand, ListingArgs, ListingFormat};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{Table, clip, json_line, print, println, status};

pub(crate) fn goal(start: &Path, command: GoalCommand) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    match command {
        GoalCommand::Add(args) => {
            let objective = args.objective.unwrap_or_else(|| args.title.clone());
            let id = graph.add_goal(&args.title, &objective, args.counter)?;
            status(&format!("added goal {id}"));
            println(&id.0.to_string())
        }
        GoalCommand::Ls(listing) => ls(&graph, &listing),
        GoalCommand::Prune(args) => {
            let node = graph.node(args.node)?;
            if node.kind != NodeKind::Goal {
                return Err(CliError::Usage(format!("{} is not a goal", args.node)));
            }
            graph.conclude(args.node, NodeStatus::Pruned, &args.reason)?;
            status(&format!("retired goal {}", args.node));
            Ok(())
        }
    }
}

fn ls(graph: &expl_core::Graph, listing: &ListingArgs) -> Result<(), CliError> {
    let goals = graph.goals()?;
    if listing.quiet {
        for goal in &goals {
            println(&goal.id.0.to_string())?;
        }
        return Ok(());
    }
    if listing.format == ListingFormat::Json {
        for goal in &goals {
            json_line(goal)?;
        }
        return Ok(());
    }
    let mut table = Table::new(&["ID", "COUNTER", "TITLE", "OBJECTIVE"]);
    for goal in &goals {
        table.row(vec![
            goal.id.to_string(),
            if goal.counter_exploration {
                "yes"
            } else {
                "no"
            }
            .to_owned(),
            clip(&goal.title, 40),
            clip(&goal.objective, 80),
        ]);
    }
    print(&table.render())
}
