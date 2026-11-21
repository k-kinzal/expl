//! `expl run`.

use std::path::Path;

use expl_core::Runner;

use crate::cli::RunArgs;
use crate::dispatch::load;
use crate::error::CliError;
use crate::observer::StatusObserver;
use crate::output::status;

pub(crate) fn run(start: &Path, args: &RunArgs) -> Result<(), CliError> {
    let context = load(start)?;
    let _lock = context.project().lock()?;
    let cycles = Runner::new(&context).run(args.cycles, &StatusObserver)?;
    status(&format!("ran {cycles} cycle(s); see `expl candidate ls`"));
    Ok(())
}
