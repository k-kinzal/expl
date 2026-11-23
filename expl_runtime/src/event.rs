//! The durable event log. An application appends events with
//! [`EventLog`], in the same SQLite transaction as the change that produced
//! them. Consumers receive them through the
//! [`Inbox`](crate::delivery::Inbox), which grants, renews and settles each
//! delivery as a [`Lease`].

use std::fmt;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Timestamp expression for the event and delivery tables.
pub const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";
/// Database clock used to lease and renew deliveries.
pub const EPOCH: &str = "CAST(strftime('%s','now') AS INTEGER)";

const HELD: &str = "status = 'leased' AND lease_token = ?3";

/// Position of an event in an explorer's log; strictly increasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub i64);

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "e{}", self.0)
    }
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

    /// Whether this lease's token is current, without touching the lease.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn is_current(self, conn: &Connection) -> rusqlite::Result<bool> {
        conn.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM deliveries
                      WHERE agent = ?1 AND event_id = ?2 AND {HELD})"
            ),
            params![self.consumer, self.event, self.token],
            |row| row.get(0),
        )
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

/// The event log in an application's SQLite database (see
/// [`store::initialize`](crate::store::initialize) for its tables).
pub struct EventLog;

impl EventLog {
    /// Append an event of `kind` about `subject`, using the caller's
    /// transaction when the connection is in one.
    ///
    /// The subject is whatever the application's events are about (a row
    /// of its own): a consumer reacts to one event per subject at a time.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn publish(
        conn: &Connection,
        kind: &str,
        subject: Option<i64>,
    ) -> rusqlite::Result<EventId> {
        conn.query_row(
            "INSERT INTO events (kind, subject) VALUES (?1, ?2) RETURNING id",
            params![kind, subject],
            |row| row.get(0).map(EventId),
        )
    }

    /// Append an event that carries `payload`, as [`publish`](Self::publish)
    /// does.
    ///
    /// # Errors
    ///
    /// SQLite or payload serialization failure.
    pub fn publish_with(
        conn: &Connection,
        kind: &str,
        subject: Option<i64>,
        payload: &impl Serialize,
    ) -> Result<EventId, EventError> {
        let payload = serde_json::to_string(payload)?;
        Ok(conn.query_row(
            "INSERT INTO events (kind, subject, payload) VALUES (?1, ?2, ?3) RETURNING id",
            params![kind, subject, payload],
            |row| row.get(0).map(EventId),
        )?)
    }

    /// The payload `event` was published with, if it has one.
    ///
    /// # Errors
    ///
    /// SQLite failure, or a payload that does not decode into `T`.
    pub fn payload<T: DeserializeOwned>(
        conn: &Connection,
        event: EventId,
    ) -> Result<Option<T>, EventError> {
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM events WHERE id = ?1",
                [event.0],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(payload
            .map(|payload| serde_json::from_str(&payload))
            .transpose()?)
    }

    /// Set how urgent `event` is: among the events a consumer can react to,
    /// the highest priority is delivered first, and the oldest among equals.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn prioritize(conn: &Connection, event: EventId, priority: i64) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE events SET priority = ?2 WHERE id = ?1",
            params![event.0, priority],
        )
        .map(drop)
    }

    /// The newest event recorded, if any.
    ///
    /// # Errors
    ///
    /// SQLite failure.
    pub fn last(conn: &Connection) -> rusqlite::Result<Option<EventId>> {
        conn.query_row("SELECT MAX(id) FROM events", [], |row| {
            row.get::<_, Option<i64>>(0).map(|id| id.map(EventId))
        })
    }
}

/// An event could not be stored or read.
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
    use crate::delivery::Inbox;

    #[test]
    fn typed_events_retry_and_reject_a_replaced_lease() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("explorer.db");
        let db = crate::store::open(&path).expect("db");
        crate::store::initialize(&db).expect("tables");
        let event =
            EventLog::publish_with(&db, "paper.found", Some(4), &vec!["arXiv"]).expect("event");
        assert_eq!(EventLog::last(&db).expect("last"), Some(event));
        assert_eq!(
            EventLog::payload::<Vec<String>>(&db, event).expect("payload"),
            Some(vec!["arXiv".to_owned()])
        );
        let bare = EventLog::publish(&db, "proof.checked", None).expect("event");
        assert_eq!(
            EventLog::payload::<Vec<String>>(&db, bare).expect("payload"),
            None
        );

        let mut inbox = Inbox::open(&path, "verify").expect("inbox");
        assert!(
            inbox
                .lease(&["survey.done"], 1, false, 3)
                .expect("filtered subscription")
                .is_none()
        );
        let first = inbox
            .lease(&["paper.found"], 1, false, 3)
            .expect("lease")
            .expect("delivery");
        assert_eq!(first.event.id, event);
        assert_eq!(first.event.subject, Some(4));
        let old = Lease {
            consumer: "verify",
            event: event.0,
            token: first.token.as_str(),
        };
        assert_eq!(
            old.settle(
                &db,
                Disposition::Retry {
                    note: "retry",
                    max_attempts: 3
                }
            )
            .expect("settle"),
            Settlement::Retrying
        );
        let second = inbox
            .lease(&["paper.found"], 2, false, 3)
            .expect("lease")
            .expect("delivery");
        assert_eq!(second.attempts, 2);
        assert!(!old.renew(&db, 60).expect("renew"));
        let active = Lease {
            consumer: "verify",
            event: event.0,
            token: second.token.as_str(),
        };
        assert_eq!(
            active
                .settle(&db, Disposition::Acknowledge(None))
                .expect("settle"),
            Settlement::Settled
        );
        assert!(
            inbox
                .lease(&["paper.found"], 3, false, 3)
                .expect("lease")
                .is_none()
        );
    }
}
