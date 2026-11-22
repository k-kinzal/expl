//! [`Evaluation`] and [`Conclusion`].

use serde::{Deserialize, Serialize};

use crate::graph::NodeId;
use crate::graph::node::text_enum;

/// The evaluator's verdict on one completed step against one goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    /// Row identity.
    pub id: i64,
    /// The evaluated step.
    pub node: NodeId,
    /// The goal it was judged against.
    pub goal: NodeId,
    /// Whether the step's branch satisfies the goal.
    pub satisfied: bool,
    /// Progress toward the goal, 0–100.
    pub score: i64,
    /// The evaluator's reasoning.
    pub reason: String,
    /// Creation time.
    pub created_at: String,
}

/// An evaluation about to be recorded.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NewEvaluation {
    /// The goal judged against.
    pub goal: NodeId,
    /// Verdict.
    pub satisfied: bool,
    /// Progress, clamped to 0–100 when recorded.
    pub score: i64,
    /// Reasoning.
    pub reason: String,
}

/// What evaluation decided about a step it judged: the step itself, before
/// the goals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// It did what it set out to do: the hypothesis it stood for holds.
    Done,
    /// Its hypothesis cannot be met this way; trying it again would not
    /// help.
    Abandoned,
    /// It stopped short of its objective, which still looks reachable: it
    /// runs again, from what it left, with the conclusion as guidance.
    Continue,
}

text_enum!(Decision { Done => "done", Abandoned => "abandoned", Continue => "continue" });

/// Evaluation's conclusion about one step: the [`Decision`], and the
/// information exploration grows the next hypotheses from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conclusion {
    /// Row identity.
    pub id: i64,
    /// The step concluded about.
    pub node: NodeId,
    /// The evaluation agent that concluded.
    pub agent: Option<String>,
    /// Done, abandoned, or continue.
    pub decision: Decision,
    /// The hypothesis the step stood for, as evaluation read it.
    pub hypothesis: String,
    /// Done: what it established. Abandoned: why the hypothesis cannot be
    /// met. Continue: how far it got and why it stopped.
    pub finding: String,
    /// Done: what changes because of it. Abandoned: what that rules out,
    /// and what it suggests instead. Continue: what the next attempt must
    /// do.
    pub consequence: String,
    /// Continue, change steps: the commit the next attempt starts from.
    pub resume_commit: Option<String>,
    /// Continue: whether the step was put back to run again (not when
    /// something already builds on it or adopted it, or it was no longer
    /// completed or failed).
    pub reopened: bool,
    /// Creation time.
    pub created_at: String,
}

/// A conclusion about to be recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewConclusion {
    /// The evaluation agent concluding.
    pub agent: String,
    /// Done, abandoned, or continue.
    pub decision: Decision,
    /// See [`Conclusion::hypothesis`].
    pub hypothesis: String,
    /// See [`Conclusion::finding`].
    pub finding: String,
    /// See [`Conclusion::consequence`].
    pub consequence: String,
}
