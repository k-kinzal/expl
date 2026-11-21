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
            let id = graph.add_goal(&args.title, &objective, args.counter, &args.under)?;
            if args.under.is_empty() {
                status(&format!("added goal {id}"));
            } else {
                let parents: Vec<String> = args.under.iter().map(ToString::to_string).collect();
                status(&format!("added goal {id} under {}", parents.join(", ")));
            }
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
    let tree = graph.goal_tree()?;
    let refs: Vec<&expl_core::Node> = goals.iter().collect();
    let goals = tree.ordered(&refs);
    let under = |goal: &expl_core::Node| -> Vec<i64> {
        tree.parents(goal.id).iter().map(|p| p.0).collect()
    };
    if listing.quiet {
        for goal in &goals {
            println(&goal.id.0.to_string())?;
        }
        return Ok(());
    }
    if listing.format == ListingFormat::Json {
        for goal in &goals {
            let mut value = serde_json::to_value(goal)
                .map_err(|e| CliError::io("encode json", std::io::Error::other(e)))?;
            value["under"] = serde_json::json!(under(goal));
            json_line(&value)?;
        }
        return Ok(());
    }
    let mut table = Table::new(&["ID", "UNDER", "STATUS", "COUNTER", "TITLE", "OBJECTIVE"]);
    for goal in &goals {
        let parents = under(goal);
        let depth = tree.depth(goal.id);
        table.row(vec![
            goal.id.to_string(),
            if parents.is_empty() {
                "-".to_owned()
            } else {
                parents
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            },
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
            clip(&format!("{}{}", "  ".repeat(depth), goal.title), 40),
            clip(&goal.objective, 80),
        ]);
    }
    print(&table.render())
}
