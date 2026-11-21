//! [`Context`] — the collaborators every reaction shares.

use crate::agent::Agent;
use crate::declaration::{Declaration, DeclarationError};
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
        }
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

    /// The declared agent `name`, brought to life.
    ///
    /// # Errors
    ///
    /// [`DeclarationError::UnknownAgent`], or
    /// [`DeclarationError::Timeout`] for an unusable `timeout`.
    pub fn agent(&self, name: &str) -> Result<Agent, DeclarationError> {
        Agent::from_declaration(name, self.declaration.agent(name)?)
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
