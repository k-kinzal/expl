//! Agents: which AI agent CLI runs, and how it is invoked.
//!
//! Mirrors iter's three levels, one direction of knowledge:
//!
//! - [`Agent`] — the noun. A struct, not a trait: it realises the
//!   invocation cycle once — scratch directory, spawn in the workspace
//!   directory, prompt delivery on stdin, faithful capture teed into the
//!   run log, interpretation.
//! - [`AgentDriver`] — the bidirectional translator for one CLI:
//!   [`command`](AgentDriver::command) encodes expl's intent (prompt,
//!   response schema) into the CLI's dialect, and
//!   [`interpret`](AgentDriver::interpret) decodes the CLI's verdict back
//!   into an [`AgentRun`]. Drivers never touch a process.
//! - The concrete drivers in [`drivers`], named after the product they
//!   translate for.
//!
//! Unlike iter, every expl invocation asks for a **structured response**:
//! each role hands the agent a JSON Schema and reads the reply as data.

pub mod drivers;

// Defining module named for the concept it defines — the path echo is deliberate.
mod agent;
mod driver;
mod error;
mod kind;
pub mod response;
mod run;

pub use agent::{Agent, Invocation};
pub use driver::{AgentCommand, AgentDriver, AgentRequest};
pub use drivers::{ClaudeCodeDriver, CodexDriver, GenericDriver};
pub use error::AgentError;
pub use kind::AgentKind;
pub use run::AgentRun;
