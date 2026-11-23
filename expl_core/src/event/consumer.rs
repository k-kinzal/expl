//! A configured consumer combines an event subscription with its execution backend.

use crate::agent::Agent;
use crate::declaration::{ConsumerDeclaration, DeclarationError};

/// A subscription and its invocation state, owned by one worker.
#[derive(Debug)]
pub struct Consumer {
    name: String,
    declaration: ConsumerDeclaration,
    agent: Agent,
}

impl Consumer {
    /// Bind a declared event consumer to the commands it can invoke.
    ///
    /// # Errors
    ///
    /// [`DeclarationError::Timeout`] if a command has an invalid timeout.
    pub(crate) fn new(
        name: &str,
        declaration: &ConsumerDeclaration,
    ) -> Result<Self, DeclarationError> {
        let agent =
            Agent::new(name, declaration.env.clone(), declaration.clis()).map_err(|source| {
                DeclarationError::Timeout {
                    consumer: name.to_owned(),
                    source,
                }
            })?;
        Ok(Self {
            name: name.to_owned(),
            declaration: declaration.clone(),
            agent,
        })
    }

    pub(crate) fn with_cluster(mut self, cluster: expl_runtime::cluster::Cluster) -> Self {
        self.agent = self.agent.with_cluster(cluster);
        self
    }

    /// The name used to track subscriptions, deliveries and processes.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The subscription and the settings for its handler.
    #[must_use]
    pub fn declaration(&self) -> &ConsumerDeclaration {
        &self.declaration
    }

    /// Invoke the program selected for the reaction.
    pub(crate) fn agent(&self) -> &Agent {
        &self.agent
    }
}
