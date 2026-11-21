//! [`StatusObserver`] — renders runner events as status lines on stderr.

use std::fmt::Write;

use expl_core::RunnerEvent;
use expl_core::RunnerObserver;
use expl_core::role::{EvaluationReport, ExecutionReport, ExplorationReport, PlanningReport};

use crate::output::{clip, short_commit, status};

/// See the [module docs](self).
#[derive(Debug, Default)]
pub(crate) struct StatusObserver;

fn ids(ids: &[expl_core::NodeId]) -> String {
    if ids.is_empty() {
        "-".to_owned()
    } else {
        ids.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub(crate) fn describe_exploration(report: &ExplorationReport) -> String {
    let Some(from) = report.from else {
        return format!("explore {}: nothing to explore from", report.direction);
    };
    let mut line = format!(
        "explore {} from {from}: proposed {}",
        report.direction,
        ids(&report.proposed)
    );
    for rejection in &report.rejected {
        let _ = write!(
            line,
            "\n  rejected \"{}\": {}",
            clip(&rejection.title, 60),
            rejection.reason
        );
    }
    line
}

pub(crate) fn describe_planning(report: &PlanningReport) -> String {
    format!(
        "plan {}: {} path(s), planned {}, pruned {} — {}",
        report.plan.0,
        report.paths,
        ids(&report.planned),
        ids(&report.pruned),
        clip(&report.summary, 160)
    )
}

pub(crate) fn describe_execution(report: &ExecutionReport) -> String {
    format!(
        "{} {} on {} ({}) — {}",
        report.node,
        report.status,
        report.branch,
        short_commit(report.head_commit.as_deref()),
        clip(&report.summary, 160)
    )
}

pub(crate) fn describe_evaluation(report: &EvaluationReport) -> String {
    if report.verdicts.is_empty() {
        return format!("{}: no goals to evaluate against", report.node);
    }
    report
        .verdicts
        .iter()
        .map(|v| {
            format!(
                "{} vs {}: {}/100{}",
                report.node,
                v.goal,
                v.score,
                if v.satisfied { " SATISFIED" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl RunnerObserver for StatusObserver {
    fn observe(&self, event: &RunnerEvent<'_>) {
        match event {
            RunnerEvent::Recovered(nodes) => {
                status(&format!("re-planned steps left running: {}", ids(nodes)));
            }
            RunnerEvent::CycleStarted(n) => status(&format!("── cycle {n} ──")),
            RunnerEvent::Explored(report) => status(&describe_exploration(report)),
            RunnerEvent::ExplorationDeferred { pending } => {
                status(&format!("explore: deferred ({pending} steps pending)"));
            }
            RunnerEvent::Planned(report) => status(&describe_planning(report)),
            RunnerEvent::Executing(nodes) => status(&format!("execute {} …", ids(nodes))),
            RunnerEvent::Executed(report) => status(&describe_execution(report)),
            RunnerEvent::Evaluating(nodes) => status(&format!("evaluate {} …", ids(nodes))),
            RunnerEvent::Evaluated(report) => status(&describe_evaluation(report)),
            RunnerEvent::RoleFailed { role, node, error } => status(&match node {
                Some(node) => format!("{role} failed on {node}: {error}"),
                None => format!("{role} failed: {error}"),
            }),
            RunnerEvent::Idle => status("nothing left to do; stopping"),
        }
    }
}
