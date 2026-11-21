//! `expl logs <NODE>` — the newest agent log about a node.

use std::path::Path;

use crate::cli::LogsArgs;
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{print, status};

pub(crate) fn logs(start: &Path, args: &LogsArgs) -> Result<(), CliError> {
    let (_, graph) = open(start)?;
    graph.node(args.node)?;
    let runs = graph.agent_runs(Some(args.node), 1)?;
    let Some(run) = runs.first() else {
        status(&format!("no agent runs for {}", args.node));
        return Ok(());
    };
    let Some(log_path) = &run.log_path else {
        return Err(CliError::Usage(format!(
            "agent run {} has no log",
            run.id.0
        )));
    };
    status(&format!(
        "agent run {} ({} via {}, {}): {log_path}",
        run.id.0, run.role, run.agent, run.status
    ));
    let text = std::fs::read_to_string(log_path)
        .map_err(|e| CliError::io(format!("read {log_path}"), e))?;
    print(&text)
}
