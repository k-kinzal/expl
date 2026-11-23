//! Producers record graph changes as immutable [`Event`]s. A [`Consumer`]
//! subscribes to event kinds, leases deliveries from its [`Inbox`], and
//! can produce further events by recording a handler's result.
//!
//! Each consumer has its own inbox. A delivery group records one eligible
//! recipient per event before issuing a delivery. Replicas compete for
//! deliveries, with one lease at a time for events about the same node
//! within a consumer.
//!
//! A delivery is a lease: it is acknowledged only after the reaction
//! succeeds. If the process dies, the lease expires and the event is
//! delivered again. Every lease has its own [`LeaseToken`]: after expiry,
//! taking over the same event replaces it, and leasing a conflicting event
//! revokes it. Renewing, acknowledging, failing, releasing, or coalescing
//! a delivery requires the current token, which stays valid after
//! expiry until it is replaced, revoked, or settled
//! ([`InboxError::LeaseLost`]).
//!
// Defining module named for the concept it defines — the path echo is deliberate.
mod consumer;
mod event;
mod inbox;

pub use consumer::Consumer;
pub use event::{Event, EventId, EventKind};
pub(crate) use inbox::Lease;
pub use inbox::{Delivery, Inbox, InboxError, LEASE_SECONDS, LeaseToken};

/// A deterministic lease token for tests that create worktrees without an
/// inbox.
#[cfg(test)]
pub(crate) fn test_token(value: u128) -> LeaseToken {
    LeaseToken::new(format!("{value:032x}"))
}
