//! What this explorer executes for the runtime: one reaction of a
//! consumer's handler to the event a worker leased
//! ([`react`]), and the revisit an idle exploration makes ([`revisit`]).
//!
//! The runtime's worker owns the delivery: it leases the event, runs the
//! command registered for the consumer, and settles the delivery from the
//! [`Outcome`] the command reports. This is that command's work. It makes
//! the handler's reaction and says what became of it; it never settles the
//! delivery itself.

use opentelemetry::trace::TraceContextExt;
use opentelemetry::{Context as Trace, KeyValue};
use thiserror::Error;

use expl_runtime::reaction::{Mode, Outcome, Reaction as Leased};

use crate::event::{Consumer, Delivery};
use crate::graph::{Graph, GraphError, NodeStatus};
use crate::handler::{Abandonment, Context, HandlerError, Reaction, Report};
use crate::process::{Interrupt, ProcessId};
use crate::telemetry::{self, node_ids};

/// The reaction could not be made at all: what the worker described is not
/// an event of this graph, or the graph cannot be used.
#[derive(Debug, Error)]
pub enum ReactError {
    /// The event is not one the graph records.
    #[error("cannot react: {0}")]
    Event(String),
    /// The graph failed.
    #[error(transparent)]
    Graph(#[from] GraphError),
}

/// React, as `consumer`, to the delivery a worker leased, or clean up after
/// a delivery that ended for good ([`Mode::Abandon`]), and say what became
/// of it. `interrupt` is the reaction's: raised when the worker stops, and
/// as a lost lease when the event went to another holder.
///
/// # Errors
///
/// [`ReactError`] when no reaction could be made; the worker counts it as
/// a failed attempt.
pub fn react(
    context: &Context,
    consumer: &Consumer,
    leased: &Leased,
    interrupt: &Interrupt,
) -> Result<Outcome, ReactError> {
    let handler = consumer.declaration().handler;
    let delivery = Delivery::try_from(leased.delivery.clone()).map_err(ReactError::Event)?;
    let mut graph = context.open_graph()?;
    if leased.mode == Mode::Abandon {
        let why = format!(
            "abandoned: the process reacting to {} died on attempt {} of {}",
            delivery.event, delivery.attempts, leased.max_attempts
        );
        return Ok(match handler.abandon(&delivery, &mut graph, &why)? {
            Abandonment::Settled(_) => settled_elsewhere(&delivery),
            abandoned @ (Abandonment::Nothing | Abandonment::Failed(_)) => Outcome::Retry {
                note: abandoned
                    .failed()
                    .map_or_else(String::new, |node| format!("{node} failed")),
            },
        });
    }
    let reaction = Reaction::new(
        context,
        consumer,
        ProcessId(leased.worker),
        &delivery,
        interrupt,
    );
    let result = handler.react(&reaction, &mut graph);
    Ok(match result {
        _ if interrupt.lease_lost() => Outcome::Lost,
        Err(HandlerError::Graph(GraphError::LeaseLost(_))) => Outcome::Lost,
        Ok(report) => {
            let kind = record(&report);
            Outcome::Acknowledge {
                note: Some(report.summary()),
                covered: report.covered().map(|event| event.0),
                kind: (kind != ACKNOWLEDGED).then(|| kind.to_owned()),
            }
        }
        // The worker is stopping: the event goes back, and the attempt does
        // not count.
        Err(error) if interrupt.is_raised() => Outcome::Release {
            note: Some(error.to_string()),
        },
        // A busy database is transient (another process held the lock past
        // the busy timeout): give the event back rather than count a failure.
        Err(HandlerError::Graph(error)) if error.is_busy() => Outcome::Release {
            note: Some(HandlerError::Graph(error).to_string()),
        },
        // The step was settled elsewhere meanwhile: there is nothing left
        // to do, and retrying would change nothing.
        Err(HandlerError::Graph(error))
            if delivery.event.node.is_some_and(|id| error.is_settled(id)) =>
        {
            settled_elsewhere(&delivery)
        }
        // Any other failure, a graph error included (often a race with
        // another process), is counted and retried. On the last attempt the
        // step is failed first, so that dying before the delivery is given
        // up leaves it to be abandoned rather than lost.
        Err(error) => {
            telemetry::error(&Trace::current(), error.telemetry_kind());
            let why = error.to_string();
            let abandoned = if leased.is_last_attempt() {
                handler.abandon(&delivery, &mut graph, &why)?
            } else {
                Abandonment::Nothing
            };
            match abandoned {
                Abandonment::Settled(_) => settled_elsewhere(&delivery),
                Abandonment::Nothing | Abandonment::Failed(_) => Outcome::Retry { note: why },
            }
        }
    })
}

/// The acknowledgement of a delivery whose step was settled elsewhere: no
/// reaction is left to make.
fn settled_elsewhere(delivery: &Delivery) -> Outcome {
    let step = delivery
        .event
        .node
        .map_or_else(|| "its step".to_owned(), |id| id.to_string());
    let report = Report::Ignored {
        reason: format!("{step} was settled elsewhere"),
    };
    Outcome::Acknowledge {
        note: Some(report.summary()),
        covered: None,
        kind: Some(record(&report).to_owned()),
    }
}

/// Record `node.revisited` for a random starting point (steps not carried
/// out yet among them when exploring `ahead`); `false` when there is none
/// (no active goals).
///
/// # Errors
///
/// [`GraphError`] on graph failure.
pub fn revisit(graph: &mut Graph, ahead: bool) -> Result<bool, GraphError> {
    let points = graph.starting_points(ahead)?;
    if points.is_empty() {
        return Ok(false);
    }
    let node = points[random_below(points.len())].id;
    telemetry::operation(
        "expl.exploration.revisit",
        vec![
            KeyValue::new("expl.node.id", node.0),
            KeyValue::new("expl.exploration.speculate", ahead),
        ],
        "graph",
        || graph.revisit(node, ahead),
    )?;
    Ok(true)
}

/// A number in `0..bound` (`bound` > 0), random enough to spread
/// explorations over the starting points.
fn random_below(bound: usize) -> usize {
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is seeded randomly per instance.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    (hasher.finish() % bound as u64) as usize
}

/// What a plain acknowledgement is counted as.
const ACKNOWLEDGED: &str = "acknowledged";

/// Describe `report` on the current span, and name the kind of
/// acknowledgement it is for the delivery metrics.
fn record(report: &Report) -> &'static str {
    let context = Trace::current();
    let span = context.span();
    if !span.is_recording() {
        return acknowledgement(report);
    }
    match report {
        Report::Exploration { explorations } => {
            span.set_attributes([
                KeyValue::new("expl.exploration.searches", explorations.len() as i64),
                KeyValue::new(
                    "expl.exploration.proposed_count",
                    explorations
                        .iter()
                        .map(|search| search.proposed.len() as i64)
                        .sum::<i64>(),
                ),
                KeyValue::new(
                    "expl.exploration.rejected_count",
                    explorations
                        .iter()
                        .map(|search| search.rejected.len() as i64)
                        .sum::<i64>(),
                ),
            ]);
            for search in explorations {
                span.add_event(
                    "expl.exploration.result",
                    vec![
                        KeyValue::new("expl.exploration.direction", search.direction.as_str()),
                        KeyValue::new("expl.exploration.from.id", search.from.0),
                        KeyValue::new(
                            "expl.exploration.proposed_ids",
                            node_ids(search.proposed.iter().copied()),
                        ),
                        KeyValue::new(
                            "expl.exploration.rejected_count",
                            search.rejected.len() as i64,
                        ),
                    ],
                );
            }
        }
        Report::Planning(plan) => {
            span.set_attributes([
                KeyValue::new("expl.plan.id", plan.plan.0),
                KeyValue::new("expl.plan.paths", plan.paths as i64),
                KeyValue::new("expl.plan.planned_count", plan.planned.len() as i64),
                KeyValue::new("expl.plan.joined_count", plan.joined.len() as i64),
                KeyValue::new("expl.plan.rebased_count", plan.rebased.len() as i64),
                KeyValue::new("expl.plan.pruned_count", plan.pruned.len() as i64),
            ]);
            if let Some(covered) = plan.covered {
                span.set_attribute(KeyValue::new("expl.plan.covered_through.id", covered.0));
            }
            for (action, nodes) in [
                ("planned", &plan.planned),
                ("joined", &plan.joined),
                ("rebased", &plan.rebased),
                ("pruned", &plan.pruned),
            ] {
                span.add_event(
                    "expl.plan.action",
                    vec![
                        KeyValue::new("expl.plan.action", action),
                        KeyValue::new("expl.plan.node_ids", node_ids(nodes.iter().copied())),
                        KeyValue::new("expl.plan.node_count", nodes.len() as i64),
                    ],
                );
            }
        }
        Report::Verification(verification) => {
            span.set_attribute(KeyValue::new(
                "expl.verification.status",
                verification.status.as_str(),
            ));
            if !verification.branch.is_empty() {
                span.set_attribute(KeyValue::new(
                    "expl.verification.branch",
                    verification.branch.clone(),
                ));
            }
            if let Some(commit) = &verification.head_commit {
                span.set_attribute(KeyValue::new("expl.verification.commit", commit.clone()));
            }
            if let Some(answer) = verification.answer {
                span.set_attribute(KeyValue::new("expl.investigation.answer", answer.as_str()));
            }
        }
        Report::Evaluation(evaluation) => {
            span.set_attributes([
                KeyValue::new("expl.evaluation.reopened", evaluation.reopened),
                KeyValue::new(
                    "expl.evaluation.verdict_count",
                    evaluation.verdicts.len() as i64,
                ),
                KeyValue::new(
                    "expl.evaluation.satisfied_count",
                    evaluation
                        .verdicts
                        .iter()
                        .filter(|verdict| verdict.satisfied)
                        .count() as i64,
                ),
            ]);
            if let Some(decision) = evaluation.decision {
                span.set_attribute(KeyValue::new("expl.evaluation.decision", decision.as_str()));
            }
            for verdict in &evaluation.verdicts {
                span.add_event(
                    "expl.evaluation.verdict",
                    vec![
                        KeyValue::new("expl.goal.id", verdict.goal.0),
                        KeyValue::new("expl.evaluation.score", verdict.score),
                        KeyValue::new("expl.evaluation.satisfied", verdict.satisfied),
                    ],
                );
            }
        }
        Report::Ignored { reason } => {
            span.set_attribute(KeyValue::new(
                "expl.delivery.ignored_reason",
                reason.clone(),
            ));
        }
    }
    acknowledgement(report)
}

fn acknowledgement(report: &Report) -> &'static str {
    match report {
        Report::Ignored { .. } => "ignored",
        Report::Verification(report) if report.status == NodeStatus::Failed => {
            "verification_failed"
        }
        Report::Exploration { .. }
        | Report::Planning(_)
        | Report::Verification(_)
        | Report::Evaluation(_) => ACKNOWLEDGED,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use expl_runtime::delivery::Delivery as Untyped;

    use super::*;
    use crate::declaration::Declaration;
    use crate::event::{EventKind, Inbox};
    use crate::graph::Answer;
    use crate::handler::Handler;
    use crate::process::MAX_ATTEMPTS;
    use crate::project::Project;
    use crate::store::{self, EPOCH};

    /// A project in `dir` with the default declaration, and its verifier.
    fn verifier(dir: &Path) -> (Context, Consumer) {
        let declaration = Declaration::parse(Declaration::TEMPLATE).expect("declaration");
        let context = Context::new(Project::at(dir), declaration);
        std::fs::create_dir_all(context.project().state_dir()).expect("state");
        let consumer = context.consumer("verifier").expect("verifier");
        (context, consumer)
    }

    /// The delivery as a worker hands it to the command that cleans up
    /// after its dead holder.
    fn abandoned(delivery: &Delivery) -> Leased {
        Leased {
            consumer: "verifier".to_owned(),
            delivery: Untyped {
                event: expl_runtime::delivery::Event {
                    id: delivery.event.id,
                    kind: delivery.event.kind.as_str().to_owned(),
                    subject: delivery.event.node.map(|node| node.0),
                    created_at: delivery.event.created_at.clone(),
                },
                attempts: delivery.attempts,
                token: delivery.token.clone(),
            },
            max_attempts: MAX_ATTEMPTS,
            worker: 1,
            turn: 0,
            mode: Mode::Abandon,
            outcome: None,
        }
    }

    fn expire(db: &Path) {
        store::open(db)
            .expect("db")
            .execute(
                &format!("UPDATE deliveries SET leased_until = {EPOCH} - 1"),
                [],
            )
            .expect("expire");
    }

    #[test]
    fn a_verification_whose_holders_keep_dying_fails_its_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (context, consumer) = verifier(dir.path());
        let db = context.project().database_path();
        let mut graph = Graph::open(&db).expect("graph");
        let origin = graph
            .create_origin("o", Some("main"), "abc")
            .expect("origin");
        let goal = graph.add_goal("g", "g", false, &[]).expect("goal");
        let step = graph
            .propose_step("s", "s", &[origin], &[goal])
            .expect("step");
        graph.plan_step(step, 1).expect("plan"); // node.ready
        let role = Handler::Verification;
        let mut inbox = Inbox::open(&db, "verifier").expect("inbox");
        let mut last = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let delivery = inbox
                .lease(role.events(), ProcessId(attempt), false, MAX_ATTEMPTS)
                .expect("lease")
                .expect("node.ready");
            assert_eq!(
                (delivery.event.kind, delivery.event.node, delivery.attempts),
                (EventKind::NodeReady, Some(step), attempt)
            );
            // Each holder claims the step, then dies without settling.
            assert!(graph.claim(step, &delivery.token).expect("claim"));
            expire(&db);
            let dead = inbox
                .abandoned(role.events(), MAX_ATTEMPTS)
                .expect("abandoned");
            if attempt < MAX_ATTEMPTS {
                assert!(dead.is_empty(), "attempts are left");
                assert_eq!(graph.node(step).expect("step").status, NodeStatus::Running);
            } else {
                assert_eq!(dead, std::slice::from_ref(&delivery));
            }
            last = Some(delivery);
        }
        let last = last.expect("leased");

        // The worker has the command clean up, then gives the delivery up.
        let outcome = react(
            &context,
            &consumer,
            &abandoned(&last),
            &Interrupt::default(),
        )
        .expect("abandon");
        assert_eq!(
            outcome,
            Outcome::Retry {
                note: format!("{step} failed")
            }
        );
        assert!(inbox.give_up_abandoned(&last).expect("give up"));

        let failed = graph.node(step).expect("step");
        assert_eq!(failed.status, NodeStatus::Failed);
        let events = graph.events(None, 100).expect("events");
        let newest = events.last().expect("an event");
        assert_eq!(
            (newest.kind, newest.node),
            (EventKind::NodeFailed, Some(step))
        );
        assert_eq!(
            inbox.status_of(last.event.id).expect("status"),
            Some("failed".into())
        );
        assert!(
            inbox
                .lease(role.events(), ProcessId(9), false, MAX_ATTEMPTS)
                .expect("lease")
                .is_none(),
            "neither the given-up event nor anything else is delivered"
        );
    }

    #[test]
    fn an_abandoned_delivery_whose_step_was_settled_is_acknowledged_as_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (context, consumer) = verifier(dir.path());
        let db = context.project().database_path();
        let mut graph = Graph::open(&db).expect("graph");
        let origin = graph
            .create_origin("o", Some("main"), "abc")
            .expect("origin");
        let goal = graph.add_goal("g", "g", false, &[]).expect("goal");
        let question = graph
            .propose_investigation("q", "is it so?", &[origin], &[goal])
            .expect("investigation");
        graph.plan_step(question, 1).expect("plan"); // node.ready
        let role = Handler::Verification;
        let mut inbox = Inbox::open(&db, "verifier").expect("inbox");
        let mut last = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let delivery = inbox
                .lease(role.events(), ProcessId(attempt), false, MAX_ATTEMPTS)
                .expect("lease")
                .expect("node.ready");
            if attempt == MAX_ATTEMPTS {
                // The last holder records its finding, then dies before
                // acknowledging the event.
                assert!(graph.claim(question, &delivery.token).expect("claim"));
                graph
                    .record_finding(question, &delivery.token, "abc", Answer::Yes, "seen it")
                    .expect("finding");
            }
            expire(&db);
            last = Some(delivery);
        }
        let last = last.expect("leased");
        let answered = graph.node(question).expect("node");
        let before = graph.last_event().expect("last");

        let outcome = react(
            &context,
            &consumer,
            &abandoned(&last),
            &Interrupt::default(),
        )
        .expect("abandon");
        let Outcome::Acknowledge { note, kind, .. } = outcome else {
            panic!("the delivery was not acknowledged: {outcome:?}");
        };
        let note = note.expect("a note");
        assert!(note.starts_with("ignored: "), "{note}");
        assert_eq!(kind.as_deref(), Some("ignored"));
        assert_eq!(graph.node(question).expect("node"), answered);
        assert_eq!(graph.last_event().expect("last"), before, "no node.failed");
    }

    #[test]
    fn a_delivery_abandoned_while_another_holds_its_step_leaves_the_step_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (context, consumer) = verifier(dir.path());
        let db = context.project().database_path();
        let mut graph = Graph::open(&db).expect("graph");
        let origin = graph
            .create_origin("o", Some("main"), "abc")
            .expect("origin");
        let goal = graph.add_goal("g", "g", false, &[]).expect("goal");
        let step = graph
            .propose_step("s", "s", &[origin], &[goal])
            .expect("step");
        graph.plan_step(step, 1).expect("plan"); // node.ready
        let role = Handler::Verification;
        // Another consumer, `fast`, is carrying the step out.
        let mut fast = Inbox::open(&db, "fast").expect("inbox");
        let owner = fast
            .lease(role.events(), ProcessId(9), false, MAX_ATTEMPTS)
            .expect("lease")
            .expect("node.ready");
        assert!(graph.claim(step, &owner.token).expect("claim"));
        let mut slow = Inbox::open(&db, "verifier").expect("inbox");
        let mut last = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let delivery = slow
                .lease(role.events(), ProcessId(attempt), false, MAX_ATTEMPTS)
                .expect("lease")
                .expect("node.ready");
            // Its holders keep dying before they get to the step.
            store::open(&db)
                .expect("db")
                .execute(
                    &format!(
                        "UPDATE deliveries SET leased_until = {EPOCH} - 1 WHERE agent = 'verifier'"
                    ),
                    [],
                )
                .expect("expire");
            last = Some(delivery);
        }
        let outcome = react(
            &context,
            &consumer,
            &abandoned(&last.expect("leased")),
            &Interrupt::default(),
        )
        .expect("abandon");
        assert_eq!(
            outcome,
            Outcome::Retry {
                note: String::new()
            },
            "nothing of `fast`'s was failed"
        );
        assert_eq!(graph.node(step).expect("step").status, NodeStatus::Running);
    }

    #[test]
    fn random_below_stays_in_bounds_and_spreads() {
        let mut seen = [0usize; 4];
        for _ in 0..400 {
            seen[random_below(4)] += 1;
        }
        assert!(seen.iter().all(|&n| n > 0), "{seen:?}");
        assert_eq!(random_below(1), 0);
    }
}
