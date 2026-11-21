//! Rendering the graph as text for prompts.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::graph::{Evaluation, GoalTree, Node, NodeId, NodeKind, Snapshot};

/// Longest excerpt of free text (outcomes, reasons) quoted per node.
const EXCERPT: usize = 600;

pub(crate) fn excerpt(text: &str) -> String {
    excerpt_to(text, EXCERPT)
}

/// `text` flattened to one line of at most `width` characters.
pub(crate) fn excerpt_to(text: &str, width: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= width {
        flat
    } else {
        let cut: String = flat.chars().take(width).collect();
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

/// Goals as a task states them: title and objective, no ids.
pub(crate) fn goals(goals: &[Node]) -> String {
    let mut out = String::new();
    for goal in goals {
        let _ = writeln!(out, "- {}", goal.title);
        if !goal.objective.trim().is_empty() {
            let _ = writeln!(out, "  {}", goal.objective.trim());
        }
    }
    out
}

/// What a subgoal is, said once wherever subgoals are listed.
pub(crate) const SUBGOALS: &str = "A goal marked \"subgoal of\" is a hypothesis about a route \
     toward the goals it serves: a stage or precondition expected to bring them within reach. \
     Work toward a subgoal counts toward the goals it serves, but those can be reached without \
     it, and several subgoals of one goal are alternative routes, not a checklist.\n";

/// Goals with the ids a response refers to them by, subgoals indented under
/// the goals they serve.
pub(crate) fn numbered_goals(goals: &[Node], tree: &GoalTree) -> String {
    if goals.is_empty() {
        return "(none)\n".to_owned();
    }
    let refs: Vec<&Node> = goals.iter().collect();
    let shown: Vec<NodeId> = goals.iter().map(|g| g.id).collect();
    let mut out = String::new();
    let mut nested = false;
    for goal in tree.ordered(&refs) {
        let depth = tree
            .ancestors(goal.id)
            .iter()
            .filter(|a| shown.contains(a))
            .count()
            .min(tree.depth(goal.id));
        let indent = "  ".repeat(depth);
        let _ = write!(out, "{indent}- [{}] {}", goal.id.0, goal.title);
        let parents = tree.parents(goal.id);
        if !parents.is_empty() {
            nested = true;
            let list: Vec<String> = parents.iter().map(|p| format!("[{}]", p.0)).collect();
            let _ = write!(out, " (subgoal of {})", list.join(", "));
        }
        out.push('\n');
        if !goal.objective.trim().is_empty() {
            let _ = writeln!(out, "{indent}  {}", excerpt(&goal.objective));
        }
    }
    if nested {
        let _ = write!(out, "\n{SUBGOALS}");
    }
    out
}

/// The steps for planning: every non-pruned node of `snapshot` with its
/// edges, outcome, and latest evaluation per goal.
pub(crate) fn graph(snapshot: &Snapshot) -> String {
    let mut after: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
    let mut before: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
    for edge in &snapshot.edges {
        after.entry(edge.target).or_default().push(edge.source);
        before.entry(edge.source).or_default().push(edge.target);
    }
    let mut latest: BTreeMap<(NodeId, NodeId), &Evaluation> = BTreeMap::new();
    for evaluation in &snapshot.evaluations {
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
    for node in &snapshot.nodes {
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
            if let Some(served) = before.get(&node.id) {
                let _ = writeln!(out, "    subgoal of: {}", ids(Some(served)));
            }
        } else {
            let _ = writeln!(
                out,
                "    after: {}   before: {}",
                ids(after.get(&node.id)),
                ids(before.get(&node.id))
            );
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
    out
}
