//! Printing a process log, optionally following it.

use std::io::{Read, Seek, SeekFrom};
use std::thread;
use std::time::Duration;

use expl_core::{ProcessRecord, Processes};

use crate::error::CliError;
use crate::output::print;

/// Print `record`'s log; with `follow`, keep printing until it exits.
pub(crate) fn print_log(
    table: &Processes,
    record: &ProcessRecord,
    follow: bool,
) -> Result<(), CliError> {
    let path = &record.log_path;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !follow => return Ok(()),
        Err(error) => return Err(CliError::io(format!("open {path}"), error)),
    };
    let mut position = 0;
    loop {
        file.seek(SeekFrom::Start(position))
            .map_err(|e| CliError::io(format!("read {path}"), e))?;
        let mut chunk = String::new();
        let read = file
            .read_to_string(&mut chunk)
            .map_err(|e| CliError::io(format!("read {path}"), e))?;
        position += read as u64;
        if !chunk.is_empty() {
            print(&chunk)?;
        }
        if !follow {
            return Ok(());
        }
        if read == 0 && !table.get(record.id)?.is_live() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(300));
    }
}
