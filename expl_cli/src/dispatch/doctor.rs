//! `expl doctor` — check the graph against git, and with `--fix` repair
//! what is safe to repair.

use std::path::Path;

use expl_core::{Doctor, Inconsistency, Repair};
use serde_json::json;

use crate::cli::{DoctorArgs, ListingFormat};
use crate::dispatch::open;
use crate::error::CliError;
use crate::output::{Table, json_line, print, status};

pub(crate) fn doctor(start: &Path, args: &DoctorArgs) -> Result<(), CliError> {
    let (project, mut graph) = open(start)?;
    let doctor = Doctor::new(&project);
    let found = doctor.examine(&graph)?;
    let repairs = if args.fix {
        doctor.repair(&mut graph, &found)?
    } else {
        Vec::new()
    };
    let repair_of = |inconsistency: &Inconsistency| {
        repairs
            .iter()
            .find(|repair| &repair.inconsistency == inconsistency)
    };

    if args.format == ListingFormat::Json {
        for inconsistency in &found {
            json_line(&json!({
                "inconsistency": inconsistency,
                "repairable": inconsistency.is_repairable(),
                "repair": repair_of(inconsistency).map(|r| json!({
                    "repaired": r.repaired,
                    "note": r.note,
                })),
            }))?;
        }
    } else if !found.is_empty() {
        let mut header = vec!["NODE", "PROBLEM", "DETAIL"];
        if args.fix {
            header.push("REPAIR");
        }
        let mut table = Table::new(&header);
        for inconsistency in &found {
            let mut row = vec![
                inconsistency.node().to_string(),
                inconsistency.kind().to_owned(),
                inconsistency.to_string(),
            ];
            if args.fix {
                row.push(repair_of(inconsistency).map_or_else(|| "-".to_owned(), describe));
            }
            table.row(row);
        }
        print(&table.render())?;
    }

    let remaining = if args.fix {
        doctor.examine(&graph)?.len()
    } else {
        found.len()
    };
    if found.is_empty() {
        status("the graph agrees with git");
    } else if !args.fix {
        let repairable = found.iter().filter(|i| i.is_repairable()).count();
        if repairable > 0 {
            status(&format!(
                "{repairable} of them can be repaired with `expl doctor --fix`"
            ));
        }
    }
    if remaining > 0 {
        return Err(CliError::Inconsistent(remaining));
    }
    Ok(())
}

fn describe(repair: &Repair) -> String {
    if repair.repaired {
        format!("fixed: {}", repair.note)
    } else {
        format!("not fixed: {}", repair.note)
    }
}
