//! [`ActivityLog`] — renders a process's activity as timestamped lines.
//!
//! A foreground process writes them to stderr and to its log file; a
//! detached process's stderr *is* its log file.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use expl_core::{ProcessActivity, ProcessObserver};

/// See the [module docs](self).
#[derive(Debug)]
pub(crate) struct ActivityLog {
    name: String,
    tee: Option<Mutex<File>>,
    /// The last pause reason printed, so a paused process logs it once.
    paused: Mutex<Option<String>>,
}

impl ActivityLog {
    /// Lines for process `name` on stderr, and also into `tee` when given.
    pub(crate) fn new(name: &str, tee: Option<File>) -> Self {
        Self {
            name: name.to_owned(),
            tee: tee.map(Mutex::new),
            paused: Mutex::new(None),
        }
    }

    fn line(&self, message: &str) {
        let line = format!("{} {} | {message}\n", timestamp(), self.name);
        std::io::stderr().lock().write_all(line.as_bytes()).ok();
        if let Some(tee) = &self.tee {
            let mut file = tee
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            file.write_all(line.as_bytes()).ok();
        }
    }
}

impl ProcessObserver for ActivityLog {
    fn observe(&self, activity: &ProcessActivity<'_>) {
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match activity {
            ProcessActivity::Paused(reason) => {
                if paused.as_deref() != Some(*reason) {
                    *paused = Some((*reason).to_owned());
                    self.line(reason);
                }
                return;
            }
            ProcessActivity::Followed(advance) => {
                let adopted = if advance.adopted.is_empty() {
                    "nothing adopted".to_owned()
                } else {
                    format!(
                        "adopted {}",
                        advance
                            .adopted
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                };
                self.line(&format!(
                    "trunk {} moved to {}: origin {} supersedes {}, {adopted}{}",
                    advance.branch,
                    advance.commit.chars().take(7).collect::<String>(),
                    advance.origin,
                    advance.previous,
                    if advance.descends {
                        ""
                    } else {
                        " (the trunk was rewritten)"
                    }
                ));
                return;
            }
            ProcessActivity::Unfollowed(reason) => {
                self.line(&format!("cannot follow the trunk: {reason}"));
                return;
            }
            ProcessActivity::Started(record) => self.line(&format!(
                "started process {} of agent `{}` (pid {})",
                record.id,
                record.agent,
                std::process::id()
            )),
            ProcessActivity::Reacting(delivery) => {
                let attempt = if delivery.attempts > 1 {
                    format!(" (attempt {})", delivery.attempts)
                } else {
                    String::new()
                };
                self.line(&format!("reacting to {}{attempt}", delivery.event));
            }
            ProcessActivity::Reacted { delivery, report } => {
                self.line(&format!("{}: {}", delivery.event.id, report.summary()));
            }
            ProcessActivity::Failed {
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
            ProcessActivity::Exiting(reason) => self.line(&format!("exiting: {reason}")),
        }
        *paused = None;
    }
}

/// Now as `YYYY-MM-DDTHH:MM:SSZ`.
pub(crate) fn timestamp() -> String {
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
