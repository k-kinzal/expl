//! Command invocation for event handlers.
//!
//! An [`Agent`] runs a command for one reaction. Its caller provides the
//! prompt, response schema and workspace. The command can be an AI CLI or
//! a program that computes a response itself.
//!
//! Translating for a particular CLI is the job of an [`AgentDriver`] — the
//! bidirectional translator kept from iter: [`command`](AgentDriver::command)
//! encodes the request (prompt, response schema) into the CLI's dialect,
//! [`interpret`](AgentDriver::interpret) decodes the CLI's verdict into an
//! [`AgentRun`]. [`Agent::invoke`] spawns the command in the workspace, sends the prompt,
//! logs its output, handles interruptions and decodes the response.
//!
//! Every invocation asks for a **structured response**: the handler hands the
//! agent a JSON Schema and reads the reply as data.

pub mod drivers;

// Defining module named for the concept it defines — the path echo is deliberate.
mod agent;
mod driver;
mod error;
pub mod response;
mod run;

pub use agent::{Agent, Invocation};
pub use driver::{AgentCommand, AgentDriver, AgentRequest, DriverKind};
pub use drivers::{ClaudeCodeDriver, CodexDriver, CursorAgentDriver, GenericDriver};
pub use error::AgentError;
pub use run::AgentRun;
