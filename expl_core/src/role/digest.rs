//! Rendering the graph as text for prompts.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::graph::{Evaluation, GoalTree, Node, NodeId, NodeKind, Snapshot};

/// Longest excerpt of free text (step objectives, outcomes, reasons) quoted per node.
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

/// What kind of node it is, as prompts name it: `origin`, `goal`, `step`
/// (a change) or `investigation`.
pub(crate) fn kind(node: &Node) -> &'static str {
    if node.kind == NodeKind::Step && node.investigation {
        "investigation"
    } else {
        node.kind.as_str()
    }
}

/// What the two kinds of step are, said once wherever steps are proposed or
/// planned.
pub(crate) const STEP_KINDS: &str = "Steps come in two kinds. A `change` step changes the code, \
     and the result can be adopted into it. An \
     `investigation` step changes nothing: it answers a question about the code or the world \
     (\"is this repository affected by CVE-…?\", \"does any caller rely on this behaviour?\", \
     \"why did that step fail?\") with a finding — yes, no or unknown, with its evidence — that \
     counts toward the goals it heads for. Investigations are cheap ways to test a hypothesis \
     before building on it.\n";

/// Completed investigations among `nodes`, with what they found.
pub(crate) fn findings(nodes: &[Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        if node.investigation && node.status == crate::graph::NodeStatus::Completed {
            let answer = node.answer.map_or("unknown", |a| a.as_str());
            let _ = writeln!(out, "- {} — {}: {answer}", node.id, node.title);
            if let Some(evidence) = &node.outcome {
                let _ = writeln!(out, "  evidence: {}", excerpt(evidence));
            }
        }
    }
    out
}

/// One completed investigation's finding with its evidence complete: every
/// line as recorded, nothing cut, so a decisive file reference or command
/// result is shown in its original form however long the evidence runs.
/// Each line is indented under the headline; an empty line stays empty.
pub(crate) fn finding_in_full(node: &Node) -> String {
    let answer = node.answer.map_or("unknown", |a| a.as_str());
    let mut out = format!("- {} — {}: {answer}\n", node.id, node.title);
    if let Some(evidence) = &node.outcome {
        out.push_str("  evidence (complete, as recorded):\n");
        for line in evidence.trim_end().lines() {
            if line.trim().is_empty() {
                out.push('\n');
            } else {
                let _ = writeln!(out, "    {line}");
            }
        }
    }
    out
}

/// One line identifying a node.
pub(crate) fn headline(node: &Node) -> String {
    let mut line = format!("{} {} [{}", node.id, kind(node), node.status);
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

/// Goals with their full objectives and the ids a response refers to them by,
/// subgoals indented under the goals they serve.
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
            let _ = writeln!(out, "{indent}  {}", goal.objective);
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
        if node.kind == NodeKind::Goal {
            let _ = writeln!(out, "    objective: {}", node.objective);
            let _ = writeln!(out, "    reached from: {}", ids(after.get(&node.id)));
            if let Some(served) = before.get(&node.id) {
                let _ = writeln!(out, "    subgoal of: {}", ids(Some(served)));
            }
        } else {
            if node.kind == NodeKind::Step {
                let _ = writeln!(out, "    objective: {}", excerpt(&node.objective));
            }
            let _ = writeln!(
                out,
                "    after: {}   before: {}",
                ids(after.get(&node.id)),
                ids(before.get(&node.id))
            );
        }
        if let Some(failed) = node.derived_from {
            let _ = writeln!(out, "    grown from the failure of: {failed}");
        }
        if let Some(answer) = node.answer {
            let _ = writeln!(out, "    finding: {answer}");
        }
        if let Some(outcome) = &node.outcome {
            let label = if node.answer.is_some() {
                "evidence"
            } else {
                "outcome"
            };
            let _ = writeln!(out, "    {label}: {}", excerpt(outcome));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declaration::Declaration;
    use crate::event::{EventKind, Inbox};
    use crate::git::Git;
    use crate::graph::{Graph, NewEvaluation, NewOrigin};
    use crate::process::{Interrupt, Processes};
    use crate::project::Project;
    use crate::role::{Context, Reaction, Role};

    #[test]
    fn role_prompts_include_the_full_goal_objective() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = Git::new(dir.path());
        git.run(&["init", "-q", "-b", "main"]).expect("init");
        for title in ["Start", "Advance"] {
            git.run(&[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                title,
            ])
            .expect("commit");
        }
        let project = Project::at(dir.path());
        std::fs::create_dir(project.state_dir()).expect("state directory");
        let db = project.database_path();
        let mut graph = Graph::open(&db).expect("graph");
        let previous = graph
            .create_origin(
                "Start",
                Some("main"),
                &git.resolve_commit("HEAD^").expect("base"),
            )
            .expect("origin");
        let final_sentence = "\n\nVerify every acceptance criterion.";
        let objective = format!(
            "{}{final_sentence}",
            "x".repeat(2_000 - final_sentence.len())
        );
        let goal = graph
            .add_goal("Full objective", &objective, false, &[])
            .expect("goal");
        graph
            .advance_origin(&NewOrigin {
                previous,
                title: "Advance".into(),
                note: String::new(),
                branch: Some("main".into()),
                commit: git.resolve_commit("HEAD").expect("head"),
                adopted: Vec::new(),
            })
            .expect("advance origin");

        let mut processes = Processes::open(&db, &project.logs_dir()).expect("processes");
        let interrupt = Interrupt::default();
        let cases = [
            (
                "explorer",
                Role::Exploration,
                "breadth = 1",
                r#"{"nodes":[]}"#,
                None,
            ),
            (
                "surveyor",
                Role::Exploration,
                "breadth = 1\nscope = \"graph\"",
                r#"{"nodes":[]}"#,
                Some("## Map"),
            ),
            (
                "planner",
                Role::Planning,
                "",
                r#"{"summary":"No steps"}"#,
                Some("## Steps"),
            ),
            (
                "evaluator",
                Role::Evaluation,
                "",
                r#"{"evaluations":[]}"#,
                None,
            ),
        ];
        for (name, role, options, reply, map_heading) in cases {
            let declaration = Declaration::parse(&format!(
                r#"
                [agent.{name}]
                role = "{role}"
                driver = "generic"
                command = ["sh", "-c", '''cat >/dev/null; printf '%s\n' "$1"''', "agent", '{reply}']
                replicas = 1
                {options}
                "#
            ))
            .expect("declaration");
            let context = Context::new(project.clone(), declaration);
            let agent = context.agent(name).expect("agent");
            let process = processes.create(name, None, false).expect("process");
            let delivery = Inbox::open(&db, name)
                .expect("inbox")
                .lease(&[EventKind::OriginAdvanced], process.id, false, 1)
                .expect("lease")
                .expect("origin event");
            let reaction = Reaction::new(&context, &agent, process.id, &delivery, &interrupt);
            role.react(&reaction, &mut graph).expect("reaction");
            let runs = graph.agent_runs(None, 1).expect("agent runs");
            assert_eq!(runs[0].agent, name);
            let log = std::fs::read_to_string(runs[0].log_path.as_ref().expect("log path"))
                .expect("run log");
            let (_, run) = log.split_once("=== prompt ===\n").expect("prompt header");
            let (prompt, _) = run.split_once("\n=== output ===").expect("prompt end");

            assert!(
                prompt.contains(&format!("- [{}] Full objective\n  {objective}\n", goal.0)),
                "{name} should receive the full objective in the goal list"
            );
            if let Some(heading) = map_heading {
                let (_, map) = prompt.split_once(heading).expect("graph section");
                assert!(
                    map.contains(&format!("    objective: {objective}\n")),
                    "{name} should receive the full objective in the graph"
                );
            }
        }
    }

    #[test]
    fn graph_excerpts_step_objectives_outcomes_and_reasons() {
        let mut graph = Graph::in_memory().expect("graph");
        let origin = graph.create_origin("Start", None, "abc").expect("origin");
        let goal = graph
            .add_goal("Goal", "Objective", false, &[])
            .expect("goal");
        let objective = "a".repeat(EXCERPT + 1);
        let outcome = "b".repeat(EXCERPT + 1);
        let reason = "c".repeat(EXCERPT + 1);
        let step = graph
            .propose_step("Step", &objective, &[origin], &[goal])
            .expect("step");
        graph.complete(step, &outcome).expect("complete");
        graph
            .record_evaluations(
                step,
                &[NewEvaluation {
                    goal,
                    satisfied: false,
                    score: 10,
                    reason: reason.clone(),
                }],
            )
            .expect("evaluation");

        let rendered = super::graph(&graph.snapshot().expect("snapshot"));
        for (label, value) in [
            ("objective: ".to_owned(), objective),
            ("outcome: ".to_owned(), outcome),
            (
                format!("evaluation vs {goal}: score 10/100, not satisfied — "),
                reason,
            ),
        ] {
            assert!(rendered.contains(&format!("    {label}{}…\n", &value[..EXCERPT])));
            assert!(!rendered.contains(&value));
        }
    }
}
