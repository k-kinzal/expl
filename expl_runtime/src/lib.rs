//! Infrastructure for event-driven exploration applications.
//!
//! Applications own their events, delivery policy and state transitions.
//! This crate provides worker lifecycles, command drivers and execution on
//! one machine or across members sharing a cluster directory.

#![deny(rust_2018_idioms)]

pub mod agent;
pub mod cluster;
pub mod command;
pub mod event;
pub mod git;
mod interrupt;
pub mod prompt;
pub mod telemetry;
pub mod testing;
pub mod worker;

pub use interrupt::Interrupt;
pub use prompt::Prompt;
