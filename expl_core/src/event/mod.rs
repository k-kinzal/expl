//! Events: what happened to the graph, and how agents receive it.
//!
//! expl is event-driven. Every change to the graph — a goal added by the
//! human, a step proposed, planned, completed, evaluated — is recorded as an
//! immutable [`Event`] in the same transaction as the change itself. Events
//! are the only thing that sets agents in motion: an agent reacts to the
//! [`EventKind`]s of its role, and its reaction changes the graph, which
//! records new events for other agents.
//!
//! Each declared agent has its own [`Inbox`]. For explicitly routed roles,
//! the inbox records one recipient before issuing a delivery. Otherwise
//! agents see every event of the kinds they react to. The processes of
//! each agent compete for deliveries while respecting reaction limits.
//! Events about the same node go to one process of an agent at a time.
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
//! Agents that share a role by weight each still receive every event of
//! it, but only one of them reacts. Before reacting or acknowledging an
//! event as routed elsewhere, a process records which of them the event is
//! assigned to ([`Inbox::assign`]); the first record stands, so processes
//! that loaded different weights, or restarted with new ones, agree.

// Defining module named for the concept it defines — the path echo is deliberate.
mod event;
mod inbox;

pub use event::{Event, EventId, EventKind};
pub(crate) use inbox::Lease;
pub use inbox::{Delivery, Inbox, InboxError, LEASE_SECONDS, LeaseToken};
