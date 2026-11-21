//! Rendering the graph as text for prompts.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::graph::{Evaluation, Graph, GraphError, Node, NodeFilter, NodeId, NodeKind};

/// Longest excerpt of free text (outcomes, reasons) quoted per node.
const EXCERPT: usize = 600;

pub(crate) fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= EXCERPT {
        flat
    } else {
        let cut: String = flat.chars().take(EXCERPT).collect();
        format!("{cut}…")
    }
}

/// One line identifying a node.
pub(crate) fn headline(node: &Node) -> String {
    let mut line = format!("{} {} [{}", node.id, node.kind, node.status);
    if node.kind == NodeKind::Step {
        let _ = write!(line, ", priority {}", node.priority);
    }
    line.push(']');
    let _ = write!(line, " {}", node.title);
    line
}

/// The goals, one block each.
pub(crate) fn goals(goals: &[Node]) -> String {
    if goals.is_empty() {
        return "(no active goals)\n".to_owned();
    }
    let mut out = String::new();
    for goal in goals {
        let _ = writeln!(out, "- {} {}", goal.id, goal.title);
        let _ = writeln!(out, "  {}", excerpt(&goal.objective));
        if goal.counter_exploration {
            let _ = writeln!(out, "  (counter-exploration enabled)");
        }
    }
    out
}

/// The whole graph: every non-pruned node with its edges, outcome, and
/// latest evaluation per goal.
pub(crate) fn graph(graph: &Graph) -> Result<String, GraphError> {
    let nodes = graph.nodes(&NodeFilter::default())?;
    let edges = graph.edges()?;
    let mut after: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
    let mut before: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
    for edge in &edges {
        after.entry(edge.target).or_default().push(edge.source);
        before.entry(edge.source).or_default().push(edge.target);
    }
    let mut latest: BTreeMap<(NodeId, NodeId), Evaluation> = BTreeMap::new();
    for evaluation in graph.evaluations()? {
        latest.insert((evaluation.node, evaluation.goal), evaluation);
    }
    let ids = |list: Option<&Vec<NodeId>>| {
        list.map_or_else(
            || "-".to_owned(),
            |l| {
                l.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        )
    };

    let mut out = String::new();
    let mut pruned = Vec::new();
    for node in &nodes {
        if node.status == crate::graph::NodeStatus::Pruned {
            pruned.push(node.id.to_string());
            continue;
        }
        let _ = writeln!(out, "{}", headline(node));
        if node.kind != NodeKind::Origin {
            let _ = writeln!(out, "    objective: {}", excerpt(&node.objective));
        }
        if node.kind == NodeKind::Goal {
            let _ = writeln!(out, "    reached from: {}", ids(after.get(&node.id)));
        } else {
            let _ = writeln!(
                out,
                "    after: {}   before: {}",
                ids(after.get(&node.id)),
                ids(before.get(&node.id))
            );
        }
        if let Some(branch) = &node.branch {
            let _ = writeln!(out, "    branch: {branch}");
        }
        if let Some(outcome) = &node.outcome {
            let _ = writeln!(out, "    outcome: {}", excerpt(outcome));
        }
        for ((_, goal), evaluation) in
            latest.range((node.id, NodeId(i64::MIN))..=(node.id, NodeId(i64::MAX)))
        {
            let _ = writeln!(
                out,
                "    evaluation vs {goal}: score {}/100, {} — {}",
                evaluation.score,
                if evaluation.satisfied {
                    "satisfied"
                } else {
                    "not satisfied"
                },
                excerpt(&evaluation.reason)
            );
        }
    }
    if !pruned.is_empty() {
        let _ = writeln!(out, "(pruned: {})", pruned.join(", "));
    }
    Ok(out)
}
