//! [`ActivityLog`] — renders a worker's activity as timestamped lines.
//!
//! A foreground worker writes them to stderr and to its log file; a
//! detached worker's stderr *is* its log file.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use opentelemetry::Context;
use opentelemetry::trace::TraceContextExt;

use crate::worker::{Activity, Observer};

/// See the [module docs](self).
#[derive(Debug)]
pub struct ActivityLog {
    name: String,
    tee: Option<Mutex<File>>,
}

impl ActivityLog {
    /// Lines for process `name` on stderr, and also into `tee` when given.
    #[must_use]
    pub fn new(name: &str, tee: Option<File>) -> Self {
        Self {
            name: name.to_owned(),
            tee: tee.map(Mutex::new),
        }
    }

    fn line(&self, message: &str) {
        let context = Context::current();
        let span = context.span();
        let trace = span.span_context();
        let correlation = if trace.is_sampled() {
            format!(" trace_id={} span_id={}", trace.trace_id(), trace.span_id())
        } else {
            String::new()
        };
        let line = format!("{} {}{correlation} | {message}\n", timestamp(), self.name);
        std::io::stderr().lock().write_all(line.as_bytes()).ok();
        if let Some(tee) = &self.tee {
            let mut file = tee
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            file.write_all(line.as_bytes()).ok();
        }
    }
}

impl Observer for ActivityLog {
    fn observe(&self, activity: &Activity<'_>) {
        match activity {
            // What a task printed is the line: the trunk moved, or cannot
            // be followed.
            Activity::Task { line, .. } => self.line(line),
            Activity::Started(record) => self.line(&format!(
                "started process {} for consumer `{}` (pid {})",
                record.id,
                record.consumer,
                std::process::id()
            )),
            Activity::Reacting(delivery) => {
                let attempt = if delivery.attempts > 1 {
                    format!(" (attempt {})", delivery.attempts)
                } else {
                    String::new()
                };
                self.line(&format!("reacting to {}{attempt}", delivery.event));
            }
            Activity::Reacted { delivery, note, .. } => {
                self.line(&format!(
                    "{}: {}",
                    delivery.event.id,
                    note.unwrap_or("reacted")
                ));
            }
            Activity::LeaseLost(delivery) => self.line(&format!(
                "{}: lost the lease; the reaction was abandoned to the process that took it over",
                delivery.event.id
            )),
            Activity::Failed {
                delivery,
                error,
                retry,
            } => self.line(&format!(
                "{}: failed: {error} ({})",
                delivery.event.id,
                if *retry {
                    "will be delivered again"
                } else {
                    "giving up"
                }
            )),
            Activity::Abandoned { delivery, note } => self.line(&format!(
                "{}: its holder died on attempt {}; giving up{}",
                delivery.event.id,
                delivery.attempts,
                note.filter(|note| !note.is_empty())
                    .map_or_else(String::new, |note| format!(" ({note})"))
            )),
            Activity::Exiting(reason) => self.line(&format!("exiting: {reason}")),
        }
    }
}

/// Now as `YYYY-MM-DDTHH:MM:SSZ`.
fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Howard Hinnant's days-to-civil conversion (proleptic Gregorian, UTC).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn converts_days_to_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_513), (2026, 3, 1));
    }
}
