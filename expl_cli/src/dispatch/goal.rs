//! `expl goal ...`.

use std::path::Path;

use expl_core::{NodeFilter, NodeKind, NodeStatus};

use crate::cli::{GoalCommand, GoalLsArgs, ListingFormat};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{Table, clip, json_line, print, println, status};

pub(crate) fn goal(start: &Path, command: GoalCommand) -> Result<(), CliError> {
    let (_, mut graph) = open(start)?;
    match command {
        GoalCommand::Add(args) => {
            let objective = args.objective.unwrap_or_else(|| args.title.clone());
            let id = graph.add_goal(&args.title, &objective, args.counter)?;
            status(&format!("added goal {id}"));
            println(&id.0.to_string())
        }
        GoalCommand::Ls(args) => ls(&graph, &args),
        GoalCommand::Retire(args) => {
            let node = graph.node(args.node)?;
            if node.kind != NodeKind::Goal {
                return Err(CliError::Usage(format!("{} is not a goal", args.node)));
            }
            graph.prune(args.node, &format!("retired: {}", args.reason))?;
            status(&format!("retired goal {}", args.node));
            Ok(())
        }
    }
}

fn ls(graph: &expl_core::Graph, args: &GoalLsArgs) -> Result<(), CliError> {
    let listing = &args.listing;
    let goals = graph.nodes(&NodeFilter {
        kinds: vec![NodeKind::Goal],
        statuses: if args.all {
            Vec::new()
        } else {
            vec![NodeStatus::Active]
        },
    })?;
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
    let mut table = Table::new(&["ID", "STATUS", "COUNTER", "TITLE", "OBJECTIVE"]);
    for goal in &goals {
        table.row(vec![
            goal.id.to_string(),
            if goal.status == NodeStatus::Active {
                "active"
            } else {
                "retired"
            }
            .to_owned(),
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
