//! [`Runner`] — repeats cycles of explore → plan → execute → evaluate.
//!
//! One cycle:
//!
//! 1. **Explore** forward from the frontier, and backward from every goal
//!    with counter-exploration enabled. Skipped while `run.max_pending`
//!    steps await execution *and* there is other work to do, so proposals
//!    cannot outrun execution but a blocked graph still grows.
//! 2. **Plan** when there is something new to plan (proposals, fresh
//!    evaluations, fresh failures).
//! 3. **Execute** up to `run.parallelism` ready steps concurrently, each in
//!    its own worktree.
//! 4. **Evaluate** every completed step not yet evaluated, `parallelism` at
//!    a time.
//!
//! The runner stops only when asked (a cycle budget) or when a cycle finds
//! nothing at all to do. Role failures are reported and the cycle goes on;
//! only graph failures stop the run.

use std::thread;

use thiserror::Error;

use crate::graph::{GraphError, NodeId};
use crate::role::{
    Context, Direction, EvaluationReport, Evaluator, ExecutionReport, Executor, ExplorationReport,
    Explorer, Planner, PlanningReport, Role, RoleError,
};

/// The run cannot continue.
#[derive(Debug, Error)]
pub enum RunnerError {
    /// Graph failure.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// There is nothing to explore toward.
    #[error("the graph has no active goals (add one with `expl goal add`)")]
    NoGoals,
}

/// Something the runner did, for observers (the CLI renders these).
#[derive(Debug)]
pub enum RunnerEvent<'a> {
    /// Steps left running by a crashed run were re-planned.
    Recovered(&'a [NodeId]),
    /// A cycle begins (1-based).
    CycleStarted(usize),
    /// An exploration finished.
    Explored(&'a ExplorationReport),
    /// Exploration was skipped because enough steps are pending.
    ExplorationDeferred {
        /// Steps awaiting execution.
        pending: usize,
    },
    /// A planning finished.
    Planned(&'a PlanningReport),
    /// These steps start executing.
    Executing(&'a [NodeId]),
    /// An execution finished.
    Executed(&'a ExecutionReport),
    /// These steps start being evaluated.
    Evaluating(&'a [NodeId]),
    /// An evaluation finished.
    Evaluated(&'a EvaluationReport),
    /// A role failed; the cycle continues.
    RoleFailed {
        /// Which role.
        role: Role,
        /// About which node, if any.
        node: Option<NodeId>,
        /// What happened.
        error: &'a RoleError,
    },
    /// A cycle did nothing; the run stops.
    Idle,
}

/// Receives [`RunnerEvent`]s as they happen.
pub trait RunnerObserver {
    /// Observe one event.
    fn observe(&self, event: &RunnerEvent<'_>);
}

/// What one cycle did.
#[derive(Debug, Default)]
pub struct CycleReport {
    /// Explorations.
    pub explorations: Vec<ExplorationReport>,
    /// Planning, if it ran.
    pub planning: Option<PlanningReport>,
    /// Executions.
    pub executions: Vec<ExecutionReport>,
    /// Evaluations.
    pub evaluations: Vec<EvaluationReport>,
    /// Role failures.
    pub failures: usize,
}

impl CycleReport {
    /// Whether the cycle changed nothing.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.explorations.iter().all(|e| e.proposed.is_empty())
            && self.planning.is_none()
            && self.executions.is_empty()
            && self.evaluations.is_empty()
            && self.failures == 0
    }
}

/// See the [module docs](self).
#[derive(Debug, Clone)]
pub struct Runner<'a> {
    context: &'a Context,
}

impl<'a> Runner<'a> {
    /// A runner for `context`. The caller must hold the project lock.
    #[must_use]
    pub fn new(context: &'a Context) -> Self {
        Self { context }
    }

    /// Run up to `cycles` cycles (unbounded when `None`); returns the
    /// number of cycles run.
    ///
    /// # Errors
    ///
    /// [`RunnerError`] when the graph fails or has no goals.
    pub fn run(
        &self,
        cycles: Option<usize>,
        observer: &dyn RunnerObserver,
    ) -> Result<usize, RunnerError> {
        let graph = self.context.open_graph()?;
        let recovered = graph.recover_interrupted()?;
        if !recovered.is_empty() {
            observer.observe(&RunnerEvent::Recovered(&recovered));
        }
        let mut count = 0;
        while cycles.is_none_or(|limit| count < limit) {
            if graph.goals()?.is_empty() {
                return Err(RunnerError::NoGoals);
            }
            count += 1;
            observer.observe(&RunnerEvent::CycleStarted(count));
            let report = self.cycle(observer)?;
            if report.is_idle() {
                observer.observe(&RunnerEvent::Idle);
                break;
            }
        }
        Ok(count)
    }

    /// Run one cycle.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Graph`] when the graph fails.
    pub fn cycle(&self, observer: &dyn RunnerObserver) -> Result<CycleReport, RunnerError> {
        let mut graph = self.context.open_graph()?;
        let settings = &self.context.declaration().run;
        let mut report = CycleReport::default();
        let failed =
            |role: Role, node: Option<NodeId>, error: RoleError, report: &mut CycleReport| {
                if let RoleError::Graph(error) = error {
                    return Err(RunnerError::Graph(error));
                }
                observer.observe(&RunnerEvent::RoleFailed {
                    role,
                    node,
                    error: &error,
                });
                report.failures += 1;
                Ok(())
            };

        // 1. Explore.
        let pending = graph.pending_count()?;
        let other_work = graph.needs_planning()?
            || !graph.ready_steps(1)?.is_empty()
            || !graph.unevaluated_steps()?.is_empty();
        if pending >= settings.max_pending && other_work {
            observer.observe(&RunnerEvent::ExplorationDeferred { pending });
        } else {
            let explorer = Explorer::new(self.context);
            let mut starts = vec![(Direction::Forward, None)];
            for goal in graph.goals()?.into_iter().filter(|g| g.counter_exploration) {
                starts.push((Direction::Backward, Some(goal.id)));
            }
            for (direction, from) in starts {
                match explorer.explore(&mut graph, direction, from) {
                    Ok(exploration) => {
                        observer.observe(&RunnerEvent::Explored(&exploration));
                        report.explorations.push(exploration);
                    }
                    Err(error) => failed(Role::Explorer, from, error, &mut report)?,
                }
            }
        }

        // 2. Plan.
        if graph.needs_planning()? {
            match Planner::new(self.context).plan(&mut graph) {
                Ok(planning) => {
                    observer.observe(&RunnerEvent::Planned(&planning));
                    report.planning = Some(planning);
                }
                Err(error) => failed(Role::Planner, None, error, &mut report)?,
            }
        }

        // 3. Execute.
        let ready: Vec<NodeId> = graph
            .ready_steps(settings.parallelism)?
            .into_iter()
            .map(|n| n.id)
            .collect();
        if !ready.is_empty() {
            observer.observe(&RunnerEvent::Executing(&ready));
            for (id, result) in self.in_parallel(&ready, |graph, id| {
                Executor::new(self.context).execute(graph, id)
            }) {
                match result {
                    Ok(execution) => {
                        observer.observe(&RunnerEvent::Executed(&execution));
                        report.executions.push(execution);
                    }
                    Err(error) => failed(Role::Executor, Some(id), error, &mut report)?,
                }
            }
        }

        // 4. Evaluate.
        let unevaluated: Vec<NodeId> = graph
            .unevaluated_steps()?
            .into_iter()
            .map(|n| n.id)
            .collect();
        for batch in unevaluated.chunks(settings.parallelism) {
            observer.observe(&RunnerEvent::Evaluating(batch));
            for (id, result) in self.in_parallel(batch, |graph, id| {
                Evaluator::new(self.context).evaluate(graph, id)
            }) {
                match result {
                    Ok(evaluation) => {
                        observer.observe(&RunnerEvent::Evaluated(&evaluation));
                        report.evaluations.push(evaluation);
                    }
                    Err(error) => failed(Role::Evaluator, Some(id), error, &mut report)?,
                }
            }
        }
        Ok(report)
    }

    /// Apply `work` to every id on its own thread with its own graph
    /// connection; results come back in input order.
    fn in_parallel<T: Send>(
        &self,
        ids: &[NodeId],
        work: impl Fn(&mut crate::graph::Graph, NodeId) -> Result<T, RoleError> + Sync,
    ) -> Vec<(NodeId, Result<T, RoleError>)> {
        thread::scope(|scope| {
            let handles: Vec<_> = ids
                .iter()
                .map(|&id| {
                    let work = &work;
                    let context = self.context;
                    (
                        id,
                        scope.spawn(move || {
                            let mut graph = context.open_graph()?;
                            work(&mut graph, id)
                        }),
                    )
                })
                .collect();
            handles
                .into_iter()
                .map(|(id, handle)| {
                    let result = handle.join().unwrap_or_else(|_| {
                        Err(RoleError::Io(std::io::Error::other("role thread panicked")))
                    });
                    (id, result)
                })
                .collect()
        })
    }
}
