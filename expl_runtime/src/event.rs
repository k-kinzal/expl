//! Durable SQLite event delivery. Applications can use [`EventLog`] as an
//! ordered inbox for their own payloads, or use [`Lease`] from their own
//! selection transaction to implement different priorities and routing.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

/// Timestamp expression for the event and delivery tables.
pub const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";
/// Database clock used to lease and renew deliveries.
pub const EPOCH: &str = "CAST(strftime('%s','now') AS INTEGER)";

const HELD: &str = "status = 'leased' AND lease_token = ?3";

/// A leased event with data of the application's choosing.
#[derive(Debug)]
pub struct Event<T> {
    /// Stable event identifier.
    pub id: i64,
    /// Application-defined subscription name.
    pub kind: String,
    /// Application event payload.
    pub payload: T,
}

/// An event and its current lease.
#[derive(Debug)]
pub struct Delivery<T> {
    /// The event.
    pub event: Event<T>,
    /// Number of attempts, including this one.
    pub attempts: i64,
    /// Token required for renewal and settlement.
    pub token: String,
}

/// The identity of one delivery attempt across all workers.
#[derive(Debug, Clone, Copy)]
pub struct Lease<'a> {
    /// The named subscription.
    pub consumer: &'a str,
    /// The application event id.
    pub event: i64,
    /// The token returned when this lease was granted.
    pub token: &'a str,
}

/// Settle a delivery once its handler returns.
#[derive(Debug, Clone, Copy)]
pub enum Disposition<'a> {
    /// Successfully handled, with an optional annotation.
    Acknowledge(Option<&'a str>),
    /// Give this attempt back without counting it.
    Release,
    /// Retry within the attempt budget.
    Retry {
        /// The reason for retrying.
        note: &'a str,
        /// Maximum number of attempts.
        max_attempts: i64,
    },
    /// Terminal failure.
    Fail(&'a str),
}

/// Whether the attempt was settled or was already taken over.
#[derive(Debug, PartialEq, Eq)]
pub enum Settlement {
    /// Recorded the result.
    Settled,
    /// The event will be delivered again.
    Retrying,
    /// The supplied token no longer owns the delivery.
    Lost,
}

impl Lease<'_> {
    /// Grant a lease on a candidate selected under the same write transaction.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn grant(
        tx: &rusqlite::Transaction<'_>,
        consumer: &str,
        event: i64,
        worker: i64,
        attempts: i64,
        seconds: i64,
    ) -> rusqlite::Result<String> {
        let token: String =
            tx.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
        tx.execute(
            &format!(
                "INSERT INTO deliveries
                 (agent, event_id, status, process_id, attempts, leased_until, lease_token)
                 VALUES (?1, ?2, 'leased', ?3, ?4, {EPOCH} + ?5, ?6)
                 ON CONFLICT (agent, event_id) DO UPDATE SET status = 'leased',
                 process_id = ?3, attempts = ?4, leased_until = {EPOCH} + ?5,
                 lease_token = ?6, updated_at = {NOW}"
            ),
            params![consumer, event, worker, attempts, seconds, token],
        )?;
        Ok(token)
    }

    /// Extend this lease if its token is current.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn renew(self, conn: &Connection, seconds: i64) -> rusqlite::Result<bool> {
        let changed = conn.execute(
            &format!(
                "UPDATE deliveries SET leased_until = {EPOCH} + ?4
                      WHERE agent = ?1 AND event_id = ?2 AND {HELD}"
            ),
            params![self.consumer, self.event, self.token, seconds],
        )?;
        Ok(changed == 1)
    }

    /// Write the result conditionally on the current token.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn settle(
        self,
        conn: &Connection,
        disposition: Disposition<'_>,
    ) -> rusqlite::Result<Settlement> {
        let (state, note, limit) = match disposition {
            Disposition::Acknowledge(note) => ("done", note, 0),
            Disposition::Release => ("release", None, 0),
            Disposition::Retry { note, max_attempts } => ("retry", Some(note), max_attempts),
            Disposition::Fail(note) => ("failed", Some(note), 0),
        };
        let status: Option<String> = conn
            .query_row(
                &format!(
                    "UPDATE deliveries SET status = CASE ?4
                    WHEN 'release' THEN 'pending'
                    WHEN 'retry' THEN CASE WHEN attempts < ?6 THEN 'pending' ELSE 'failed' END
                    ELSE ?4 END,
                 attempts = CASE WHEN ?4 = 'release' THEN MAX(attempts - 1, 0) ELSE attempts END,
                 note = CASE WHEN ?4 = 'release' THEN note ELSE ?5 END,
                 leased_until = NULL, lease_token = NULL, updated_at = {NOW}
                 WHERE agent = ?1 AND event_id = ?2 AND {HELD} RETURNING status"
                ),
                params![self.consumer, self.event, self.token, state, note, limit],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match status.as_deref() {
            None => Settlement::Lost,
            Some("pending") => Settlement::Retrying,
            Some(_) => Settlement::Settled,
        })
    }
}

/// A generic event log over an application's SQLite connection.
pub struct EventLog;

impl EventLog {
    /// Create the event and delivery tables in the application's database.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn initialize(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY, kind TEXT NOT NULL, payload TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS events_kind ON events (kind, id);
             CREATE TABLE IF NOT EXISTS deliveries (
                agent TEXT NOT NULL, event_id INTEGER NOT NULL REFERENCES events (id),
                status TEXT NOT NULL, process_id INTEGER, attempts INTEGER NOT NULL DEFAULT 0,
                leased_until INTEGER, lease_token TEXT, note TEXT,
                updated_at TEXT NOT NULL DEFAULT ({NOW}), PRIMARY KEY (agent, event_id)
             );"
        ))
    }

    /// Append the event, using the caller's transaction when supplied.
    ///
    /// # Errors
    ///
    /// SQLite or payload serialization failure.
    pub fn publish(
        conn: &Connection,
        kind: &str,
        payload: &impl Serialize,
    ) -> Result<i64, EventError> {
        let payload = serde_json::to_string(payload)?;
        Ok(conn.query_row(
            "INSERT INTO events (kind, payload) VALUES (?1, ?2) RETURNING id",
            params![kind, payload],
            |row| row.get(0),
        )?)
    }

    /// Lease the oldest matching event for this subscription. An expired
    /// last attempt is recorded as failed. Applications with additional
    /// recovery rules can select candidates and call [`Lease::grant`] directly.
    ///
    /// # Errors
    ///
    /// SQLite or payload serialization failure.
    pub fn lease<T: DeserializeOwned>(
        conn: &mut Connection,
        consumer: &str,
        kinds: &[&str],
        worker: i64,
        max_attempts: i64,
        seconds: i64,
    ) -> Result<Option<Delivery<T>>, EventError> {
        let kinds = serde_json::to_string(kinds)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            &format!("UPDATE deliveries SET status = 'failed', lease_token = NULL,
                 leased_until = NULL, note = 'last lease expired', updated_at = {NOW}
                 WHERE agent = ?1 AND status = 'leased' AND leased_until < {EPOCH} AND attempts >= ?2"),
            params![consumer, max_attempts],
        )?;
        let row = tx
            .query_row(
                &format!(
                    "SELECT e.id, e.kind, e.payload, COALESCE(d.attempts, 0)
                FROM events e LEFT JOIN deliveries d ON d.event_id = e.id AND d.agent = ?1
                WHERE e.kind IN (SELECT value FROM json_each(?2))
                AND (d.event_id IS NULL OR d.status = 'pending'
                     OR (d.status = 'leased' AND d.leased_until < {EPOCH}))
                AND COALESCE(d.attempts, 0) < ?3 ORDER BY e.id LIMIT 1"
                ),
                params![consumer, kinds, max_attempts],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let delivery = if let Some((id, kind, payload, attempts)) = row {
            let payload = serde_json::from_str(&payload)?;
            let attempts = attempts + 1;
            let token = Lease::grant(&tx, consumer, id, worker, attempts, seconds)?;
            Some(Delivery {
                event: Event { id, kind, payload },
                attempts,
                token,
            })
        } else {
            None
        };
        tx.commit()?;
        Ok(delivery)
    }
}

/// An event or its delivery could not be stored.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    /// Database I/O or schema error.
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    /// Application event payload could not be encoded or decoded.
    #[error(transparent)]
    Payload(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_events_retry_and_reject_a_replaced_lease() {
        let mut db = Connection::open_in_memory().expect("db");
        EventLog::initialize(&db).expect("queue");
        let event = EventLog::publish(&db, "paper.found", &vec!["arXiv"]).expect("event");
        assert!(
            EventLog::lease::<Vec<String>>(&mut db, "verify", &["proof.checked"], 1, 3, 60)
                .expect("filtered subscription")
                .is_none()
        );
        let first: Delivery<Vec<String>> =
            EventLog::lease(&mut db, "verify", &["paper.found"], 1, 3, 60)
                .expect("lease")
                .expect("delivery");
        assert_eq!(first.event.payload, ["arXiv"]);
        let old = Lease {
            consumer: "verify",
            event,
            token: &first.token,
        };
        assert_eq!(
            old.settle(
                &db,
                Disposition::Retry {
                    note: "retry",
                    max_attempts: 3
                }
            )
            .unwrap(),
            Settlement::Retrying
        );
        let second: Delivery<Vec<String>> =
            EventLog::lease(&mut db, "verify", &["paper.found"], 2, 3, 60)
                .expect("lease")
                .expect("delivery");
        assert_eq!(second.attempts, 2);
        assert!(!old.renew(&db, 60).unwrap());
        let active = Lease {
            consumer: "verify",
            event,
            token: &second.token,
        };
        assert_eq!(
            active.settle(&db, Disposition::Acknowledge(None)).unwrap(),
            Settlement::Settled
        );
        assert!(
            EventLog::lease::<Vec<String>>(&mut db, "verify", &["paper.found"], 3, 3, 60)
                .unwrap()
                .is_none()
        );
    }
}
