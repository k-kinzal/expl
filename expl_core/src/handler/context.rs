//! [`Context`] — the collaborators every reaction shares.

use expl_runtime::cluster::Authority;
use expl_runtime::control::Control;

use crate::declaration::{Declaration, DeclarationError};
use crate::event::Consumer;
use crate::graph::{Graph, GraphError};
use crate::project::Project;
use crate::workspace::Workspaces;

/// A loaded project: where it lives, what the operator declared, and its
/// worktrees.
#[derive(Debug, Clone)]
pub struct Context {
    project: Project,
    declaration: Declaration,
    workspaces: Workspaces,
    authority: Option<Authority>,
}

impl Context {
    /// Bind `project` with its `declaration`.
    #[must_use]
    pub fn new(project: Project, declaration: Declaration) -> Self {
        let workspaces = Workspaces::new(&project);
        Self {
            project,
            declaration,
            workspaces,
            authority: None,
        }
    }

    /// Bind orchestration to an elected daemon.
    #[must_use]
    pub fn with_authority(mut self, authority: Authority) -> Self {
        self.authority = Some(authority);
        self
    }

    /// The elected daemon for this worker, if it was started through one.
    #[must_use]
    pub fn authority(&self) -> Option<&Authority> {
        self.authority.as_ref()
    }

    /// The project.
    #[must_use]
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// The declaration.
    #[must_use]
    pub fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// The worktrees.
    #[must_use]
    pub fn workspaces(&self) -> &Workspaces {
        &self.workspaces
    }

    /// The declared event consumer `name`, brought to life.
    ///
    /// # Errors
    ///
    /// [`DeclarationError::UnknownConsumer`], or
    /// [`DeclarationError::Timeout`] for an unusable `timeout`.
    pub fn consumer(&self, name: &str) -> Result<Consumer, DeclarationError> {
        let declared = self.declaration.consumer(name)?;
        let Some(authority) = &self.authority else {
            if !declared.agents.is_empty() {
                return Err(DeclarationError::NoDaemon(name.to_owned()));
            }
            return Consumer::new(name, declared);
        };
        let cluster = authority.cluster();
        let consumer = if declared.agents.is_empty() {
            Consumer::new(name, declared)?
        } else {
            // The daemon's operator defines the agents a consumer names;
            // read as they stand now, so a changed model applies to the
            // next reaction.
            let named = Control::load(cluster)?.agent;
            let clis =
                declared
                    .clis_among(&named)
                    .map_err(|agent| DeclarationError::UnknownAgent {
                        consumer: name.to_owned(),
                        agent,
                    })?;
            Consumer::over(name, declared, clis)?
        };
        Ok(consumer.with_cluster(cluster.clone(), &self.pool()))
    }

    /// The pool this project's agents count against under a daemon: the
    /// one it declares, else one of its own, named as the project is
    /// registered.
    #[must_use]
    pub fn pool(&self) -> String {
        self.declaration
            .pool
            .clone()
            .unwrap_or_else(|| self.project.explorer_name())
    }

    /// A fresh connection to the project's graph.
    ///
    /// # Errors
    ///
    /// [`GraphError`] when the database cannot be opened.
    pub fn open_graph(&self) -> Result<Graph, GraphError> {
        Graph::open(&self.project.database_path())
    }
}
