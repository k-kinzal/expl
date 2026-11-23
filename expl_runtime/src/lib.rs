//! Infrastructure for event-driven exploration applications.
//!
//! An explorer owns its event vocabulary and state transitions, and
//! [registers](registration) what reacts to which events. This crate owns
//! the rest: the [event log](event) and its [delivery](delivery) with
//! routing and [concurrency limits](capacity), the [workers](worker) that
//! execute a consumer's command for each event under the
//! [reaction contract](reaction), their [process table](process), command
//! drivers, and the [daemon] that hosts every registered explorer on one
//! machine or across members sharing a cluster directory.

#![deny(rust_2018_idioms)]

pub mod agent;
pub mod capacity;
pub mod cluster;
pub mod command;
pub mod daemon;
pub mod delivery;
pub mod event;
pub mod git;
mod interrupt;
pub mod process;
pub mod prompt;
pub mod reaction;
pub mod registration;
pub mod store;
pub mod telemetry;
pub mod testing;
pub mod worker;

pub use interrupt::Interrupt;
pub use prompt::Prompt;
