//! Evaluation — conclude about a step that was carried out, and judge
//! whether goals are satisfied.
//!
//! Reacts to `node.completed` and `node.failed`. The step is first judged
//! by itself: is it **done**, **abandoned** (its hypothesis cannot be met
//! this way), or to be **continued** (it stopped short — a timeout, an
//! interrupted run, a claim the working directory does not bear out — of
//! an objective that still looks reachable)? The [`Conclusion`] records
//! the hypothesis the step stood for, what was found, and what that
//! changes, for exploration to grow the next hypotheses from. A step
//! continued runs again from what it left, with the conclusion as guidance
//! ([`Graph::conclude`]); at most [`CONTINUATIONS`] times. A completed step
//! not continued is then judged against the active goals it leads to (or
//! every active goal when it is not linked to any).
//!
//! Reacts to `origin.advanced` too: the new origin — the trunk as the
//! owner left it — is judged against every active goal, so the lineage
//! shows how each adoption moved each goal. An origin's verdicts never make
//! it a candidate; candidates are steps. The
//! agent works in a scratch worktree at the step's commit, so it can build
//! and test the result without disturbing anyone; its prompt holds only the
//! goals.
//!
//! An investigation changed nothing, so its evidence is not the working
//! directory but its finding: the prompt then holds the question, the
//! finding, and the findings of the investigations before it on its path,
//! and the agent judges whether they settle each goal (a question is
//! satisfied once answered); the scratch worktree is the code they were
//! about, for checking the evidence. Recording the verdicts
//! records `node.evaluated` — for a failed step too, once concluded — which
//! sets exploration (what next from here, or in place of this failure?)
//! and planning (re-prioritise) in motion.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::event::EventKind;
use crate::graph::{
    Conclusion, Decision, GoalTree, Graph, NewConclusion, NewEvaluation, Node, NodeId, NodeKind,
    NodeStatus,
};
use crate::prompt::Prompt;
use crate::role::{Reaction, Report, RoleError, digest};

/// How many times one step is continued, at most: past that, evaluation
/// decides between done and abandoned.
pub(crate) const CONTINUATIONS: usize = 3;

/// Longest excerpt of the executor's report shown to the evaluator.
const REPORT_EXCERPT: usize = 4000;

/// What one evaluation did.
#[derive(Debug, Clone, Serialize)]
pub struct EvaluationReport {
    /// The evaluated step.
    pub node: NodeId,
    /// What was concluded about the step itself (`None` for an origin, or
    /// an agent that gave no conclusion).
    pub decision: Option<Decision>,
    /// Whether the step was put back to run again.
    pub reopened: bool,
    /// Verdicts recorded, one per goal.
    pub verdicts: Vec<Verdict>,
}

/// One recorded verdict.
#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    /// The goal.
    pub goal: NodeId,
    /// Whether it is satisfied.
    pub satisfied: bool,
    /// Progress, 0–100.
    pub score: i64,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    conclusion: Option<ConclusionResponse>,
    #[serde(default)]
    evaluations: Vec<EvaluationResponse>,
}

#[derive(Debug, Deserialize)]
struct ConclusionResponse {
    decision: Decision,
    hypothesis: String,
    finding: String,
    consequence: String,
}

#[derive(Debug, Deserialize)]
struct EvaluationResponse {
    goal: i64,
    satisfied: bool,
    score: i64,
    reason: String,
}

/// The response schema: verdicts against the goals and, for a step, the
/// conclusion about the step itself; `continue` is offered while the step
/// may still be continued.
fn schema(step: bool, may_continue: bool) -> serde_json::Value {
    let mut schema = goals_schema();
    if step {
        let decisions: &[&str] = if may_continue {
            &["done", "abandoned", "continue"]
        } else {
            &["done", "abandoned"]
        };
        schema["required"] = json!(["conclusion", "evaluations"]);
        schema["properties"]["conclusion"] = json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["decision", "hypothesis", "finding", "consequence"],
            "properties": {
                "decision": {"type": "string", "enum": decisions, "description": "done: it did what it set out to do. abandoned: its hypothesis cannot be met this way. continue: it stopped short of an objective that still looks reachable, and should run again from what it left."},
                "hypothesis": {"type": "string", "description": "The hypothesis the step stood for: what it assumed would hold or work."},
                "finding": {"type": "string", "description": "done: what it established, with the evidence. abandoned: why the hypothesis cannot be met (the cause, not just the symptom). continue: how far it got and why it stopped."},
                "consequence": {"type": "string", "description": "done: what changes because of it (what it makes possible, rules out, or the next steps must take into account). abandoned: what that rules out, and what it suggests trying instead. continue: what the next attempt must do to finish."}
            }
        });
    }
    schema
}

fn goals_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["evaluations"],
        "properties": {
            "evaluations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["goal", "satisfied", "score", "reason"],
                    "properties": {
                        "goal": {"type": "integer", "description": "The goal's number in brackets."},
                        "satisfied": {"type": "boolean", "description": "Whether the repository, as it is, satisfies the goal."},
                        "score": {"type": "integer", "minimum": 0, "maximum": 100, "description": "Progress toward the goal."},
                        "reason": {"type": "string", "description": "Evidence for the verdict, and what is still missing."}
                    }
                }
            }
        }
    })
}

pub(crate) fn react(reaction: &Reaction<'_>, graph: &mut Graph) -> Result<Report, RoleError> {
    let id = reaction.subject()?;
    let node = graph.node(id)?;
    let event = reaction.event();
    let judged = if event.kind == EventKind::OriginAdvanced {
        node.kind == NodeKind::Origin
    } else if event.kind == EventKind::NodeFailed {
        node.kind == NodeKind::Step && node.status == NodeStatus::Failed
    } else {
        node.kind == NodeKind::Step && node.status == NodeStatus::Completed
    };
    if !judged {
        return Ok(Reaction::ignore(format!(
            "{id} is a {} {}",
            node.status, node.kind
        )));
    }
    if node.kind == NodeKind::Origin {
        return judge_origin(reaction, graph, &node);
    }
    let earlier = graph.conclusions_of(id)?;
    if let Some(last) = earlier.last()
        && last.created_at > event.created_at
    {
        return Ok(Reaction::ignore(format!(
            "{id} was concluded about at {}, after this event",
            last.created_at
        )));
    }
    let failed = node.status == NodeStatus::Failed;
    let mut goals = graph.goals_reachable_from(id)?;
    if goals.is_empty() {
        goals = graph.goals()?;
    }
    let continued = earlier
        .iter()
        .filter(|c| c.decision == Decision::Continue && c.reopened)
        .count();
    let may_continue = continued < CONTINUATIONS;

    let scratch = if node.head_commit.is_some() {
        reaction.context().workspaces().scratch(&node)?
    } else {
        // Failed before it left any commit: the code it started from.
        let base = graph
            .predecessors(id)?
            .into_iter()
            .find(|p| p.head_commit.is_some())
            .map_or_else(|| graph.origin(), Ok)?;
        reaction.context().workspaces().scratch(&base)?
    };
    let schema = schema(true, may_continue);
    let tree = graph.goal_tree()?;
    let step = StepUnderJudgment {
        node: &node,
        earlier: &earlier,
        may_continue,
    };
    let prompt = if node.investigation {
        let mut path = graph.ancestors(id)?;
        path.push(node.clone());
        investigation_prompt(reaction, &step, &path, &goals, &tree, &schema)
    } else {
        prompt(reaction, Some(&step), &goals, &tree, &schema)
    };
    let response: Response =
        reaction.invoke_read_only(graph, Some(id), &scratch, &prompt, &schema)?;
    drop(scratch);
    let mut evaluations = verdicts(response.evaluations, &goals);
    let Some(conclusion) = response.conclusion else {
        // No conclusion given: the verdicts alone, as before conclusions.
        if failed {
            evaluations.clear();
        }
        graph.record_evaluations(id, &evaluations)?;
        return Ok(report(id, None, false, &evaluations));
    };
    let mut decision = conclusion.decision;
    if decision == Decision::Continue && !may_continue {
        decision = Decision::Abandoned;
    }
    // Verdicts on the goals are for work that stands: not a failure, nor
    // an attempt to be carried on.
    if failed || decision == Decision::Continue {
        evaluations.clear();
    }
    let reopened = graph.conclude(
        id,
        &NewConclusion {
            decision,
            hypothesis: conclusion.hypothesis,
            finding: conclusion.finding,
            consequence: conclusion.consequence,
        },
        &evaluations,
    )?;
    Ok(report(id, Some(decision), reopened, &evaluations))
}

/// Judge a new origin — the trunk as the owner left it — against every
/// active goal.
fn judge_origin(
    reaction: &Reaction<'_>,
    graph: &mut Graph,
    node: &Node,
) -> Result<Report, RoleError> {
    let id = node.id;
    let goals = graph.goals()?;
    let evaluations = if goals.is_empty() {
        Vec::new()
    } else {
        let scratch = reaction.context().workspaces().scratch(node)?;
        let schema = schema(false, false);
        let tree = graph.goal_tree()?;
        let prompt = prompt(reaction, None, &goals, &tree, &schema);
        let response: Response =
            reaction.invoke_read_only(graph, Some(id), &scratch, &prompt, &schema)?;
        drop(scratch);
        verdicts(response.evaluations, &goals)
    };
    graph.record_evaluations(id, &evaluations)?;
    Ok(report(id, None, false, &evaluations))
}

/// The agent's verdicts on `goals`, dropping any about other nodes.
fn verdicts(evaluations: Vec<EvaluationResponse>, goals: &[Node]) -> Vec<NewEvaluation> {
    evaluations
        .into_iter()
        .filter(|e| goals.iter().any(|g| g.id.0 == e.goal))
        .map(|e| NewEvaluation {
            goal: NodeId(e.goal),
            satisfied: e.satisfied,
            score: e.score.clamp(0, 100),
            reason: e.reason,
        })
        .collect()
}

fn report(
    id: NodeId,
    decision: Option<Decision>,
    reopened: bool,
    evaluations: &[NewEvaluation],
) -> Report {
    Report::Evaluation(EvaluationReport {
        node: id,
        decision,
        reopened,
        verdicts: evaluations
            .iter()
            .map(|e| Verdict {
                goal: e.goal,
                satisfied: e.satisfied,
                score: e.score,
            })
            .collect(),
    })
}

/// A step being concluded about, and what was concluded about it before.
struct StepUnderJudgment<'a> {
    node: &'a Node,
    earlier: &'a [Conclusion],
    may_continue: bool,
}

impl StepUnderJudgment<'_> {
    /// The first part of a step's evaluation: the step, what its executor
    /// reported, earlier conclusions, and how to conclude.
    fn section(&self) -> String {
        let node = self.node;
        let failed = node.status == NodeStatus::Failed;
        let mut out = format!(
            "## The step\n\n{id} — {title}\n\nObjective: {objective}\n\n\
             It {how}. Its executor reported:\n\n{report}\n\n",
            id = node.id,
            title = node.title,
            objective = node.objective.trim(),
            how = if failed {
                "failed"
            } else {
                "was carried out and reported done"
            },
            report = node.outcome.as_deref().map_or_else(
                || "(nothing recorded)".to_owned(),
                |o| digest::excerpt_to(o, REPORT_EXCERPT)
            ),
        );
        if !self.earlier.is_empty() {
            out.push_str("Earlier conclusions about this step, oldest first:\n\n");
            for conclusion in self.earlier {
                out.push_str(&digest::conclusion(conclusion));
            }
            out.push('\n');
        }
        out.push_str(
            "## Conclude about the step\n\n\
             Every step is a hypothesis. First judge the step itself, by what the working \
             directory shows rather than by the report, which is a claim to check. Decide:\n\
             - `done`: it did what its objective set out to do;\n\
             - `abandoned`: its hypothesis cannot be met this way, and running it again would \
               not help;\n",
        );
        if self.may_continue {
            out.push_str(
                "- `continue`: it stopped short — a timeout, an interrupted or crashed run, a \
                   merge left unresolved, a transient tool or environment failure, or a report \
                   claiming more than the working directory shows — of an objective that still \
                   looks reachable. It runs again, starting from what this attempt left, with \
                   your `finding` and `consequence` as its guidance.\n",
            );
        } else {
            let _ = std::fmt::Write::write_fmt(
                &mut out,
                format_args!(
                    "It was continued {CONTINUATIONS} times already, so it is not continued again: \
                     decide between `done` and `abandoned`.\n"
                ),
            );
        }
        out.push_str(
            "\nState the hypothesis the step stood for, what was found, and what that changes \
             (see the response format). Exploration grows the next hypotheses from these, so be \
             concrete: name files, behaviours, commands, and causes.\n\n",
        );
        if failed {
            out.push_str(
                "A failed step's work does not stand, so return no evaluations against the \
                 goals.\n\n",
            );
        } else {
            out.push_str(
                "Then, unless you decide `continue` (return no evaluations then), judge the goals \
                 below.\n\n",
            );
        }
        out
    }
}

/// The prompt for a change step or an origin: the goals to judge against,
/// with the working directory as the evidence, and for a step first the
/// step itself to conclude about.
fn prompt(
    reaction: &Reaction<'_>,
    step: Option<&StepUnderJudgment<'_>>,
    goals: &[Node],
    tree: &GoalTree,
    schema: &serde_json::Value,
) -> Prompt {
    Prompt::from(format!(
        "## Task\n\n\
         {first}Judge the repository in the working directory, as it is, against each goal below. \
         Inspect the code and run builds and tests as needed. Do not modify files, and leave the \
         working directory as you found it: remove anything a build or test writes there outside \
         ignored paths. Return one \
         evaluation per goal, referring to it by the number in brackets.\n\n\
         {step}## Goals\n\n{goals}{instructions}{response}",
        first = if step.is_some() {
            "Conclude about the step below, then "
        } else {
            ""
        },
        step = step.map(StepUnderJudgment::section).unwrap_or_default(),
        goals = digest::numbered_goals(goals, tree),
        instructions = super::instructions_section(reaction.agent()),
        response = super::response_section(schema),
    ))
}

/// The prompt for an investigation: the goals, the question it answered,
/// and the findings on its path — the evidence to judge.
///
/// The findings before it are digests; its own comes last and complete
/// (see [`findings`]).
fn investigation_prompt(
    reaction: &Reaction<'_>,
    step: &StepUnderJudgment<'_>,
    path: &[Node],
    goals: &[Node],
    tree: &GoalTree,
    schema: &serde_json::Value,
) -> Prompt {
    let node = step.node;
    Prompt::from(format!(
        "## Task\n\n\
         The step {id} was an investigation: it changed nothing, and set out to answer the \
         question \"{title}\". Conclude about it first (below). Then, unless it failed or you decide `continue`, \
         judge the findings on its path (below, its own last) against each goal below: how far they settle it. A goal that asks a question is \
         satisfied when the findings answer it with convincing evidence — whatever the answer; \
         a goal that asks for a change is not satisfied by findings alone, but they can move it \
         (by ruling routes in or out). Check the evidence against the working directory, which \
         holds the code the investigation was about, rather than taking it on trust. Do not \
         modify files. Return one evaluation per goal, referring to it by the number in \
         brackets.\n\n\
         {step}## Findings\n\n{findings}\n## Goals\n\n{goals}{instructions}{response}",
        id = node.id,
        title = node.title,
        step = step.section(),
        findings = findings(node, path),
        goals = digest::numbered_goals(goals, tree),
        instructions = super::instructions_section(reaction.agent()),
        response = super::response_section(schema),
    ))
}

/// The findings on `path` as the evaluator sees them: those before `node`
/// digested, `node`'s own last and complete — the evidence under judgment
/// is never cut, so a decisive file reference or command result past the
/// digest's length reaches the evaluator as recorded.
fn findings(node: &Node, path: &[Node]) -> String {
    let earlier: Vec<Node> = path.iter().filter(|n| n.id != node.id).cloned().collect();
    let mut out = digest::findings(&earlier);
    out.push_str(&digest::finding_in_full(node));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Answer;

    fn investigation(id: i64, title: &str, evidence: &str) -> Node {
        Node {
            id: NodeId(id),
            kind: NodeKind::Step,
            status: NodeStatus::Completed,
            title: title.to_owned(),
            objective: String::new(),
            priority: 0,
            counter_exploration: false,
            investigation: true,
            answer: Some(Answer::Yes),
            derived_from: None,
            proposed_by: None,
            branch: None,
            head_commit: None,
            outcome: Some(evidence.to_owned()),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn the_finding_under_judgment_is_shown_complete() {
        let padding = "Checked every caller of the parser in turn. ".repeat(30);
        let evidence = format!(
            "{padding}\n\nDecisive:\nsrc/parse.rs:412: `unwrap()` on user input\n\
             $ cargo test parse::overflow\n  test result: FAILED. 0 passed; 1 failed\n"
        );
        assert!(evidence.chars().count() > 2 * 600);
        let earlier = investigation(3, "Earlier", &format!("{padding}EARLIER-TAIL"));
        let node = investigation(5, "Is the parser affected?", &evidence);
        let path = [earlier, node.clone()];

        let shown = findings(&node, &path);

        // Its own evidence, every line in its original form, after the cutoff.
        for line in [
            "Decisive:",
            "src/parse.rs:412: `unwrap()` on user input",
            "$ cargo test parse::overflow",
            "  test result: FAILED. 0 passed; 1 failed",
        ] {
            assert!(
                shown.lines().any(|l| l == format!("    {line}")),
                "missing {line:?} in:\n{shown}"
            );
        }
        assert!(shown.contains(padding.trim_end()), "{shown}");
        // Shown once, last; earlier findings stay digests.
        assert_eq!(shown.matches("Is the parser affected?").count(), 1);
        assert!(shown.trim_end().ends_with("0 passed; 1 failed"), "{shown}");
        assert!(shown.starts_with("- #3 — Earlier: yes\n  evidence: "));
        assert!(!shown.contains("EARLIER-TAIL"), "{shown}");
    }
}
