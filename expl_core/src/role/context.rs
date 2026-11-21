//! [`Context`] — the collaborators every role shares.

use std::path::Path;

use serde::de::DeserializeOwned;

use crate::agent::{Agent, Invocation};
use crate::declaration::Declaration;
use crate::graph::{AgentRunStatus, Graph, GraphError, NodeId};
use crate::project::Project;
use crate::prompt::Prompt;
use crate::role::{Role, RoleError};
use crate::workspace::Workspaces;

/// A loaded project: where it lives, what the operator declared, and its
/// worktrees. Roles borrow it; it is `Sync`, so parallel executors share
/// one.
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

    /// The node worktrees.
    #[must_use]
    pub fn workspaces(&self) -> &Workspaces {
        &self.workspaces
    }

    /// A fresh connection to the project's graph (one per thread).
    ///
    /// # Errors
    ///
    /// [`GraphError`] when the database cannot be opened.
    pub fn open_graph(&self) -> Result<Graph, GraphError> {
        Graph::open(&self.project.database_path())
    }

    /// Run `role`'s agent once in `directory` and decode its response.
    ///
    /// The run is journaled in the graph (`agent_runs`) with its raw log
    /// under `.expl/logs/`, whether it succeeds or not.
    pub(crate) fn invoke<T: DeserializeOwned>(
        &self,
        graph: &Graph,
        role: Role,
        node: Option<NodeId>,
        directory: &Path,
        prompt: &Prompt,
        schema: &serde_json::Value,
    ) -> Result<T, RoleError> {
        let (name, declaration) = self.declaration.agent_for(role)?;
        let agent = Agent::from_declaration(name, declaration);
        let run_id = graph.begin_agent_run(role.as_str(), node, name)?;
        let logs = self.project.logs_dir();
        std::fs::create_dir_all(&logs)?;
        let log = logs.join(match node {
            Some(node) => format!("{:05}-{role}-node-{}.log", run_id.0, node.0),
            None => format!("{:05}-{role}.log", run_id.0),
        });
        graph.set_agent_run_log(run_id, &log)?;

        let mut env = vec![("EXPL_ROLE".to_owned(), role.as_str().to_owned())];
        if let Some(node) = node {
            env.push(("EXPL_NODE".to_owned(), node.0.to_string()));
        }
        let outcome = agent
            .run(&Invocation {
                directory,
                prompt,
                response_schema: schema,
                env: &env,
                log: &log,
            })
            .map_err(RoleError::from)
            .and_then(|run| {
                serde_json::from_value::<T>(run.response)
                    .map(|decoded| (decoded, run.session_id))
                    .map_err(|source| RoleError::Response {
                        role: role.as_str(),
                        source,
                    })
            });
        match outcome {
            Ok((decoded, session_id)) => {
                graph.finish_agent_run(
                    run_id,
                    AgentRunStatus::Succeeded,
                    session_id.as_deref(),
                    None,
                )?;
                Ok(decoded)
            }
            Err(error) => {
                graph.finish_agent_run(
                    run_id,
                    AgentRunStatus::Failed,
                    None,
                    Some(&error.to_string()),
                )?;
                Err(error)
            }
        }
    }
}
