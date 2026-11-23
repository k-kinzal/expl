//! Agents: the declared units that react to events.
//!
//! An [`Agent`] is what `expl.toml` declares under `[agent.<name>]`: a
//! [`Role`](crate::role::Role) (exploration, planning, verification,
//! evaluation), the AI CLI that carries it out, and how many processes of
//! it `expl up` keeps running. An agent is to a process what an image is to
//! a container: the definition, started any number of times.
//!
//! Each reaction to an event is one **invocation** of the agent's CLI.
//! Translating for a particular CLI is the job of an [`AgentDriver`] — the
//! bidirectional translator kept from iter: [`command`](AgentDriver::command)
//! encodes the request (prompt, response schema) into the CLI's dialect,
//! [`interpret`](AgentDriver::interpret) decodes the CLI's verdict into an
//! [`AgentRun`]. Drivers never touch a process; [`Agent::invoke`] realises
//! the invocation cycle once — spawn in the workspace directory, prompt on
//! stdin, output teed into the run log, cooperative interruption,
//! interpretation.
//!
//! Every invocation asks for a **structured response**: the role hands the
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
