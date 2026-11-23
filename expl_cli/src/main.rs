//! `expl` — the command-line surface of expl.

#![deny(rust_2018_idioms)]

mod cli;
mod daemon;
mod dispatch;
mod error;
mod observer;
mod output;
mod telemetry;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match daemon::dispatch(cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            output::report_error(&error);
            ExitCode::from(error.exit_code())
        }
    }
}
