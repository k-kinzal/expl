//! Rendering the graph as text for prompts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use crate::graph::{
    Conclusion, Decision, Evaluation, GoalTree, Node, NodeId, NodeKind, NodeStatus, Snapshot,
};

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

/// What evaluation concluded about a step, as a list item: the decision,
/// the hypothesis, what was found, and what it changes.
pub(crate) fn conclusion(conclusion: &Conclusion) -> String {
    let (found, follows) = labels(conclusion.decision);
    format!(
        "- concluded {decision}{reopened}\n  hypothesis: {hypothesis}\n  {found}: {finding}\n  \
         {follows}: {consequence}\n",
        decision = conclusion.decision,
        reopened = if conclusion.decision == Decision::Continue && !conclusion.reopened {
            " (not run again)"
        } else {
            ""
        },
        hypothesis = excerpt(&conclusion.hypothesis),
        finding = excerpt(&conclusion.finding),
        consequence = excerpt(&conclusion.consequence),
    )
}

/// What a conclusion's finding and consequence are, by decision.
pub(crate) fn labels(decision: Decision) -> (&'static str, &'static str) {
    match decision {
        Decision::Done => ("established", "what it changes"),
        Decision::Abandoned => ("why it cannot work", "what that rules out or suggests"),
        Decision::Continue => ("how far it got", "what remains"),
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
        if node.investigation && node.status == NodeStatus::Completed {
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

/// Most steps a route names in one list; the rest are counted.
const ROUTE_LIST: usize = 12;

/// How far each route toward the active goals of `snapshot` has come, in
/// tree order: what already looks to meet each goal (the trunk, or
/// completed steps by their latest verdict), how many open steps head for
/// it, and — for a subgoal that is met — the open steps built on what
/// meets it, which carry the route on toward the goals it serves. Empty
/// without active goals.
pub(crate) fn routes(snapshot: &Snapshot, tree: &GoalTree) -> String {
    let goals = snapshot.goals();
    if goals.is_empty() {
        return String::new();
    }
    let mut latest: BTreeMap<(NodeId, NodeId), &Evaluation> = BTreeMap::new();
    for evaluation in &snapshot.evaluations {
        latest.insert((evaluation.node, evaluation.goal), evaluation);
    }
    let status = |id: NodeId| snapshot.node(id).map(|n| (n.kind, n.status));
    let open = |id: NodeId| {
        matches!(
            status(id),
            Some((
                NodeKind::Step,
                NodeStatus::Proposed | NodeStatus::Planned | NodeStatus::Running
            ))
        )
    };
    let mut succs: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
    let mut toward: BTreeMap<NodeId, usize> = BTreeMap::new();
    for edge in &snapshot.edges {
        match status(edge.target).map(|(kind, _)| kind) {
            Some(NodeKind::Step) => succs.entry(edge.source).or_default().push(edge.target),
            Some(NodeKind::Goal) if open(edge.source) => {
                *toward.entry(edge.target).or_default() += 1;
            }
            _ => {}
        }
    }
    let trunk = snapshot
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Origin && n.status == NodeStatus::Active)
        .map(|n| n.id);
    let priority = |id: &NodeId| snapshot.node(*id).map_or(0, |n| n.priority);
    let listed = |mut ids: Vec<NodeId>, by: &dyn Fn(&NodeId) -> i64| {
        ids.sort_by_key(|id| (std::cmp::Reverse(by(id)), *id));
        let more = ids.len().saturating_sub(ROUTE_LIST);
        let mut text = ids
            .iter()
            .take(ROUTE_LIST)
            .map(|id| format!("{id} ({})", by(id)))
            .collect::<Vec<_>>()
            .join(", ");
        if more > 0 {
            let _ = write!(text, ", +{more} more");
        }
        text
    };

    let mut out = String::new();
    let refs: Vec<&Node> = goals.iter().collect();
    for goal in tree.ordered(&refs) {
        let met = |node: NodeId| latest.get(&(node, goal.id)).is_some_and(|e| e.satisfied);
        let reaching: Vec<NodeId> = snapshot
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Step && n.status == NodeStatus::Completed)
            .map(|n| n.id)
            .filter(|id| met(*id))
            .collect();
        let in_trunk = trunk.is_some_and(met);
        let depth = tree.ancestors(goal.id).len().min(tree.depth(goal.id));
        let indent = "  ".repeat(depth);
        let parents = tree.parents(goal.id);
        let _ = write!(out, "{indent}- [{}] {}", goal.id.0, goal.title);
        if !parents.is_empty() {
            let list: Vec<String> = parents.iter().map(|p| format!("[{}]", p.0)).collect();
            let _ = write!(out, " (subgoal of {})", list.join(", "));
        }
        out.push('\n');
        let score = |id: &NodeId| latest.get(&(*id, goal.id)).map_or(0, |e| e.score);
        let _ = writeln!(
            out,
            "{indent}  met by: {}{}",
            if in_trunk {
                "the trunk (active origin); "
            } else {
                ""
            },
            if reaching.is_empty() {
                "no step".to_owned()
            } else {
                format!("steps {} (score)", listed(reaching.clone(), &score))
            },
        );
        let _ = writeln!(
            out,
            "{indent}  open steps heading for it: {}",
            toward.get(&goal.id).copied().unwrap_or(0)
        );
        if parents.is_empty() {
            continue;
        }
        if in_trunk {
            let _ = writeln!(
                out,
                "{indent}  past it: everything that builds on the active origin"
            );
        } else if !reaching.is_empty() {
            let mut past: BTreeSet<NodeId> = BTreeSet::new();
            let mut stack = reaching.clone();
            let mut seen: BTreeSet<NodeId> = BTreeSet::new();
            while let Some(id) = stack.pop() {
                if !seen.insert(id) {
                    continue;
                }
                for next in succs.get(&id).into_iter().flatten() {
                    if open(*next) {
                        past.insert(*next);
                    }
                    stack.push(*next);
                }
            }
            let _ = writeln!(
                out,
                "{indent}  open steps past it, built on what meets it: {}",
                if past.is_empty() {
                    "none yet".to_owned()
                } else {
                    format!(
                        "{} (priority)",
                        listed(past.into_iter().collect(), &priority)
                    )
                }
            );
        }
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
        if node.status == NodeStatus::Pruned {
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
        if let Some(concluded) = snapshot.conclusion(node.id) {
            let (found, follows) = labels(concluded.decision);
            let _ = writeln!(
                out,
                "    concluded {}: {found}: {} — {follows}: {}",
                concluded.decision,
                excerpt(&concluded.finding),
                excerpt(&concluded.consequence)
            );
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
    use crate::process::{Interrupt, ProcessId, Processes};
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
        let evaluation_reply = serde_json::json!({
            "evaluations": [{"goal": goal, "satisfied": false, "score": 0, "reason": "missing"}]
        })
        .to_string();
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
                evaluation_reply.as_str(),
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
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let mut graph = Graph::open(&db).expect("graph");
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
        graph.plan_step(step, 1).expect("plan");
        let delivery = Inbox::open(&db, "verifier")
            .expect("inbox")
            .lease(&[EventKind::NodeReady], ProcessId(1), false, 3)
            .expect("lease")
            .expect("node.ready");
        assert!(graph.claim(step, &delivery.token).expect("claim"));
        graph
            .complete(step, &delivery.token, &outcome)
            .expect("complete");
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

    #[test]
    fn routes_show_what_meets_each_subgoal_and_what_goes_on_past_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("expl.db");
        let mut graph = Graph::open(&db).expect("graph");
        let origin = graph.create_origin("Start", None, "abc").expect("origin");
        let goal = graph.add_goal("Goal", "g", false, &[]).expect("goal");
        let sub = graph.add_goal("Sub", "s", false, &[goal]).expect("sub");
        let other = graph.add_goal("Other", "o", false, &[goal]).expect("other");
        let reach = graph
            .propose_step("reach", "x", &[origin], &[sub])
            .expect("step");
        let circling = graph
            .propose_step("circling", "x", &[origin], &[sub])
            .expect("step");
        graph.plan_step(reach, 1).expect("plan");
        let delivery = Inbox::open(&db, "verifier")
            .expect("inbox")
            .lease(&[EventKind::NodeReady], ProcessId(1), false, 3)
            .expect("lease")
            .expect("node.ready");
        assert!(graph.claim(reach, &delivery.token).expect("claim"));
        graph
            .complete(reach, &delivery.token, "done")
            .expect("complete");
        graph
            .record_evaluations(
                reach,
                &[
                    NewEvaluation {
                        goal: sub,
                        satisfied: true,
                        score: 90,
                        reason: String::new(),
                    },
                    NewEvaluation {
                        goal,
                        satisfied: false,
                        score: 40,
                        reason: String::new(),
                    },
                ],
            )
            .expect("evaluation");
        let past = graph
            .propose_step("past", "x", &[reach], &[goal])
            .expect("step");
        graph.plan_step(past, 7).expect("plan");

        let snapshot = graph.snapshot().expect("snapshot");
        let tree = GoalTree::of(&snapshot.nodes, &snapshot.edges);
        let routes = routes(&snapshot, &tree);
        let at = |needle: &str| {
            routes
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}: {routes}"))
        };
        // In tree order, the goal's own line first.
        assert!(at(&format!("- [{}] Goal", goal.0)) < at(&format!("  - [{}] Sub", sub.0)));
        assert!(
            routes.contains(&format!("met by: steps {reach} (90)")),
            "{routes}"
        );
        assert!(
            routes.contains(&format!(
                "open steps past it, built on what meets it: {past} (7)"
            )),
            "{routes}"
        );
        // Unmet: no step meets it, and nothing is past it.
        let other_part = &routes[at(&format!("[{}] Other", other.0))..];
        assert!(other_part.contains("met by: no step"), "{routes}");
        assert!(!other_part.contains("past it"), "{routes}");
        assert!(!routes.contains(&format!("{circling} (")), "{routes}");
    }
}
