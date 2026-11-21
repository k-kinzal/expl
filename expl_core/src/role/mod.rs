//! Roles: the four responsibilities that grow the graph.
//!
//! Each role is one agent invocation with a fixed contract — what it
//! reads from the graph, which worktree it runs in, the JSON Schema of its
//! response, and how the response is written back:
//!
//! | Role | Runs in | Reads | Writes |
//! | --- | --- | --- | --- |
//! | [`Explorer`] | the starting node's worktree | graph, goals | proposed steps + edges |
//! | [`Planner`] | the origin's worktree | graph, evaluations | plan (paths), priorities, prunes |
//! | [`Executor`] | the step's own worktree | the step and its ancestors | a commit on the step's branch |
//! | [`Evaluator`] | the step's worktree | the step, its diff, goals | evaluations |
//!
//! Exploration has a [`Direction`]: forward from a realised node toward the
//! goals, or backward from a goal (**counter-exploration**, enabled per
//! goal) to find what must hold just before it.
//!
//! Roles never print and never decide *when* they run; the
//! [`Runner`](crate::Runner) schedules them, and the CLI renders their
//! reports.

mod context;
mod digest;
mod error;
mod evaluator;
mod executor;
mod explorer;
mod planner;

pub use context::Context;
pub use error::RoleError;
pub use evaluator::{EvaluationReport, Evaluator, Verdict};
pub use executor::{ExecutionReport, Executor};
pub use explorer::{ExplorationReport, Explorer, Rejection};
pub use planner::{Planner, PlanningReport};

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// One of the four responsibilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Proposes nodes.
    Explorer,
    /// Draws paths and sets priorities.
    Planner,
    /// Realises a step in its worktree.
    Executor,
    /// Judges completed steps against goals.
    Evaluator,
}

impl Role {
    /// All roles, in cycle order.
    pub const ALL: [Role; 4] = [
        Role::Explorer,
        Role::Planner,
        Role::Executor,
        Role::Evaluator,
    ];

    /// Canonical name, as in `[role.<name>]`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explorer => "explorer",
            Self::Planner => "planner",
            Self::Executor => "executor",
            Self::Evaluator => "evaluator",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which way an exploration searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// From a realised node toward the goals.
    Forward,
    /// From a goal back toward the origin (counter-exploration).
    Backward,
}

impl Direction {
    /// Canonical name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Backward => "backward",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Direction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "forward" => Ok(Self::Forward),
            "backward" => Ok(Self::Backward),
            other => Err(format!(
                "unknown direction `{other}` (expected forward, backward)"
            )),
        }
    }
}

/// Shared opening of every role prompt.
fn preamble(role: Role, purpose: &str) -> String {
    format!(
        "You are the {role} agent of expl, an agentic tool that explores a problem space as a \
         graph. Nodes are states of a git repository: the origin (where exploration started), \
         steps (units of work, each realised as a commit on its own branch), and goals (what the \
         human wants). An edge A -> B means B builds on A. Several roles cooperate: the explorer \
         proposes steps, the planner orders them into paths and sets priorities, the executor \
         realises one step in its own git worktree, and the evaluator judges completed steps \
         against goals. Exploration never ends by itself; a human chooses among the steps that \
         satisfy a goal.\n\n{purpose}\n"
    )
}

/// Operator guidance from `[role.<name>].instructions`, if any.
fn instructions_section(context: &Context, role: Role) -> String {
    match &context.declaration().role_declaration(role).instructions {
        Some(text) if !text.trim().is_empty() => {
            format!("\n## Project instructions\n\n{}\n", text.trim())
        }
        Some(_) | None => String::new(),
    }
}

/// Closing of every role prompt.
fn response_section(schema: &serde_json::Value) -> String {
    format!(
        "\n## Response\n\nFinish with a single JSON object (no prose around it) that matches \
         this JSON Schema:\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(schema).unwrap_or_default()
    )
}
