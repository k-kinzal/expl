//! `expl explore | plan | execute | evaluate` — one role, once.

use std::path::Path;

use expl_core::role::{Evaluator, Executor, Explorer, Planner};

use crate::cli::{EvaluateArgs, ExecuteArgs, ExploreArgs, ReportArgs, ReportFormat};
use crate::dispatch::load;
use crate::error::CliError;
use crate::observer::{
    describe_evaluation, describe_execution, describe_exploration, describe_planning,
};
use crate::output::{json_line, println, status};

fn emit<T: serde::Serialize>(
    report: &ReportArgs,
    value: &T,
    describe: impl Fn(&T) -> String,
) -> Result<(), CliError> {
    match report.format {
        ReportFormat::Json => json_line(value),
        ReportFormat::Text => println(&describe(value)),
    }
}

pub(crate) fn explore(start: &Path, args: &ExploreArgs) -> Result<(), CliError> {
    let context = load(start)?;
    let _lock = context.project().lock()?;
    let mut graph = context.open_graph()?;
    let report = Explorer::new(&context).explore(&mut graph, args.direction, args.from)?;
    emit(&args.report, &report, describe_exploration)
}

pub(crate) fn plan(start: &Path) -> Result<(), CliError> {
    let context = load(start)?;
    let _lock = context.project().lock()?;
    let mut graph = context.open_graph()?;
    let report = Planner::new(&context).plan(&mut graph)?;
    println(&describe_planning(&report))
}

pub(crate) fn execute(start: &Path, args: &ExecuteArgs) -> Result<(), CliError> {
    let context = load(start)?;
    let _lock = context.project().lock()?;
    let mut graph = context.open_graph()?;
    let node = match args.node {
        Some(node) => node,
        None => graph
            .ready_steps(1)?
            .first()
            .map(|n| n.id)
            .ok_or_else(|| CliError::Usage("no planned step is ready to execute".into()))?,
    };
    status(&format!("executing {node} …"));
    let report = Executor::new(&context).execute(&mut graph, node)?;
    emit(&args.report, &report, describe_execution)
}

pub(crate) fn evaluate(start: &Path, args: &EvaluateArgs) -> Result<(), CliError> {
    let context = load(start)?;
    let _lock = context.project().lock()?;
    let mut graph = context.open_graph()?;
    let nodes = match args.node {
        Some(node) => vec![node],
        None => graph
            .unevaluated_steps()?
            .into_iter()
            .map(|n| n.id)
            .collect(),
    };
    if nodes.is_empty() {
        status("no completed step awaits evaluation");
    }
    for node in nodes {
        status(&format!("evaluating {node} …"));
        let report = Evaluator::new(&context).evaluate(&mut graph, node)?;
        emit(&args.report, &report, describe_evaluation)?;
    }
    Ok(())
}
