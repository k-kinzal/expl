//! Events: what happened to the graph, and how agents receive it.
//!
//! expl is event-driven. Every change to the graph — a goal added by the
//! human, a step proposed, planned, completed, evaluated — is recorded as an
//! immutable [`Event`] in the same transaction as the change itself. Events
//! are the only thing that sets agents in motion: an agent reacts to the
//! [`EventKind`]s of its role, and its reaction changes the graph, which
//! records new events for other agents.
//!
//! Each declared agent has its own [`Inbox`]: every agent sees every event
//! of the kinds it reacts to (fan-out), while the processes of one agent
//! compete for them (one [`Delivery`] each). That is where parallelism
//! comes from — independent events are handled concurrently by as many
//! processes as are running; events about the same node are handed to one
//! process of an agent at a time.
//!
//! A delivery is a lease: it is acknowledged only after the reaction
//! succeeds. If the process dies, the lease expires and the event is
//! delivered again.

// Defining module named for the concept it defines — the path echo is deliberate.
mod event;
mod inbox;

pub use event::{Event, EventId, EventKind};
pub use inbox::{Delivery, Inbox, LEASE_SECONDS};
