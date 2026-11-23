//! Bind the runtime's Git workspace to exploration workspace errors.

use std::path::Path;

pub(crate) use expl_runtime::git::{Start, identity_args};

use crate::git::Git;
use crate::workspace::WorkspaceError;

/// A private clone for one agent invocation.
#[derive(Debug)]
pub struct AgentClone(expl_runtime::git::AgentClone);

impl AgentClone {
    pub(crate) fn create(
        repository: &Git,
        root: &Path,
        label: &str,
        start: Start,
    ) -> Result<Self, WorkspaceError> {
        Ok(Self(expl_runtime::git::AgentClone::create(
            repository, root, label, start,
        )?))
    }

    /// The clone's directory, where the agent runs.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// The clone's own directory, which holds the checkout: files handed
    /// to the agent alongside the repository go here.
    #[must_use]
    pub fn directory(&self) -> &Path {
        self.0.directory()
    }

    /// Restore the clone to the state it had before the agent ran.
    ///
    /// # Errors
    ///
    /// Git or filesystem failure.
    pub fn reset(&self) -> Result<(), WorkspaceError> {
        Ok(self.0.reset()?)
    }

    pub(crate) fn snapshot(&self, base: &str, keep: &[String]) -> Result<String, WorkspaceError> {
        Ok(self.0.snapshot(base, keep)?)
    }
}
