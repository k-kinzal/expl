//! OpenTelemetry metrics and spans for workers and their deliveries.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Instant, SystemTime};

use opentelemetry::metrics::{Counter, Histogram, ObservableGauge};
use opentelemetry::trace::TraceContextExt;
use opentelemetry::{Context, ContextGuard, KeyValue, global};

use crate::event::{Consumer, Delivery, Inbox};
use crate::graph::NodeStatus;
use crate::handler::Report;
use crate::process::{ProcessActivity, ProcessObserver, ProcessRecord};
use crate::telemetry::{self, node_ids};

#[derive(Clone, Copy)]
pub(super) enum Phase {
    Starting,
    WaitingForEvents,
    WaitingForCapacity,
    Reacting,
    Exited,
}

const PHASES: [(Phase, &str); 5] = [
    (Phase::Starting, "starting"),
    (Phase::WaitingForEvents, "waiting_for_events"),
    (Phase::WaitingForCapacity, "waiting_for_capacity"),
    (Phase::Reacting, "reacting"),
    (Phase::Exited, "exited"),
];

struct DeliveryStart(Instant);

pub(super) struct WorkerTelemetry<'a> {
    observer: &'a dyn ProcessObserver,
    labels: [KeyValue; 2],
    process: Vec<KeyValue>,
    phase: Arc<AtomicU8>,
    _state: ObservableGauge<u64>,
    deliveries: Counter<u64>,
    duration: Histogram<f64>,
    queue_age: Histogram<f64>,
}

impl<'a> WorkerTelemetry<'a> {
    pub(super) fn new(
        observer: &'a dyn ProcessObserver,
        consumer: &Consumer,
        record: &ProcessRecord,
    ) -> Self {
        let declaration = consumer.declaration();
        let labels = [
            KeyValue::new("expl.consumer", consumer.name().to_owned()),
            KeyValue::new("expl.handler", declaration.handler.as_str()),
        ];
        let mut process = labels.to_vec();
        process.extend([
            KeyValue::new("expl.process.id", record.id.0),
            KeyValue::new("expl.process.name", record.name.clone()),
            KeyValue::new("expl.process.detached", record.detached),
            KeyValue::new("expl.consumer.replicas", declaration.replicas as i64),
        ]);
        if let Some(limit) = declaration.concurrency {
            process.push(KeyValue::new(
                "expl.consumer.concurrency",
                limit.get() as i64,
            ));
        }
        if let Some(group) = &declaration.group {
            process.push(KeyValue::new("expl.routing.group", group.clone()));
        }
        let meter = global::meter(telemetry::SCOPE);
        let phase = Arc::new(AtomicU8::new(Phase::Starting as u8));
        let current_phase = Arc::clone(&phase);
        let states = PHASES.map(|(phase, name)| {
            let [consumer, handler] = labels.clone();
            (
                phase as u8,
                [consumer, handler, KeyValue::new("expl.worker.phase", name)],
            )
        });
        let state = meter
            .u64_observable_gauge("expl.worker.state")
            .with_description("One for the current worker state, zero for other states")
            .with_callback(move |measure| {
                let current = current_phase.load(Ordering::Relaxed);
                for (phase, attributes) in &states {
                    measure.observe(u64::from(*phase == current), attributes);
                }
            })
            .build();
        Self {
            observer,
            labels,
            process,
            phase,
            _state: state,
            deliveries: meter
                .u64_counter("expl.delivery.count")
                .with_description("Settled delivery attempts, by outcome")
                .build(),
            duration: meter
                .f64_histogram("expl.delivery.duration")
                .with_description("Elapsed time from lease to settlement of an attempt")
                .with_unit("s")
                .with_boundaries(vec![0.01, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 1800.0])
                .build(),
            queue_age: meter
                .f64_histogram("expl.delivery.queue_age")
                .with_description("Age of an event when its delivery was leased")
                .with_unit("s")
                .with_boundaries(vec![0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 1800.0, 3600.0])
                .build(),
        }
    }

    pub(super) fn state(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    pub(super) fn delivery(&self, inbox: &Inbox, delivery: &Delivery) -> ContextGuard {
        let mut attributes = self.delivery_attributes(delivery);
        if let Some(age) = humantime::parse_rfc3339(&delivery.event.created_at)
            .ok()
            .and_then(|time| SystemTime::now().duration_since(time).ok())
        {
            let [consumer, handler] = self.labels.clone();
            let seconds = age.as_secs_f64();
            self.queue_age
                .record(seconds, &[consumer, handler, event_kind(delivery)]);
            attributes.push(KeyValue::new("expl.delivery.queue_age_seconds", seconds));
        }
        attributes.push(KeyValue::new("expl.delivery.outcome", "unsettled"));
        telemetry::consumer_span(attributes, inbox.cause(delivery.event.id))
            .with_value(DeliveryStart(Instant::now()))
            .attach()
    }

    fn delivery_attributes(&self, delivery: &Delivery) -> Vec<KeyValue> {
        let mut attributes = self.process.clone();
        attributes.extend([
            event_kind(delivery),
            KeyValue::new("expl.event.id", delivery.event.id.0),
            KeyValue::new("expl.event.created_at", delivery.event.created_at.clone()),
            KeyValue::new("expl.delivery.attempt", delivery.attempts),
        ]);
        if let Some(node) = delivery.event.node {
            attributes.push(KeyValue::new("expl.node.id", node.0));
        }
        attributes
    }

    fn record(&self, delivery: &Delivery, outcome: &'static str) {
        let [consumer, handler] = self.labels.clone();
        let attributes = [
            consumer,
            handler,
            event_kind(delivery),
            KeyValue::new("expl.delivery.outcome", outcome),
        ];
        self.deliveries.add(1, &attributes);
        let context = Context::current();
        if let Some(start) = context.get::<DeliveryStart>() {
            self.duration
                .record(start.0.elapsed().as_secs_f64(), &attributes);
        }
        context
            .span()
            .set_attribute(KeyValue::new("expl.delivery.outcome", outcome));
    }
}

impl ProcessObserver for WorkerTelemetry<'_> {
    fn observe(&self, activity: &ProcessActivity<'_>) {
        match activity {
            ProcessActivity::Started(_) => {
                telemetry::span("expl.worker.started", self.process.clone())
                    .span()
                    .end();
            }
            ProcessActivity::Exiting(reason) => {
                self.state(Phase::Exited);
                let outcome = if reason.starts_with("error: ") {
                    "error"
                } else {
                    reason
                };
                let context = telemetry::span("expl.worker.exited", self.process.clone());
                context.span().set_attribute(KeyValue::new(
                    "expl.worker.exit_reason",
                    outcome.to_string(),
                ));
                if outcome == "error" {
                    telemetry::error(&context, "process");
                }
            }
            ProcessActivity::Unfollowed(_) => {
                let context = telemetry::span("expl.trunk.follow", self.process.clone());
                telemetry::error(&context, "trunk");
            }
            ProcessActivity::Reacted { delivery, report } => {
                // Abandonment can acknowledge work settled elsewhere, outside
                // the loop that normally owns the span for this delivery.
                let _scope = (Context::current().get::<DeliveryStart>().is_none()).then(|| {
                    telemetry::consumer_span(self.delivery_attributes(delivery), None).attach()
                });
                self.record(delivery, record_report(report));
            }
            ProcessActivity::Failed {
                delivery,
                error,
                retry,
            } => {
                self.record(delivery, if *retry { "retry" } else { "failed" });
                let context = Context::current();
                context
                    .span()
                    .set_attribute(KeyValue::new("expl.delivery.retry", *retry));
                telemetry::error(&context, error.telemetry_kind());
            }
            ProcessActivity::LeaseLost(delivery) => {
                self.record(delivery, "lease_lost");
                telemetry::error(&Context::current(), "lease_lost");
            }
            ProcessActivity::Abandoned { delivery, failed } => {
                let context = telemetry::consumer_span(self.delivery_attributes(delivery), None);
                let _scope = context.clone().attach();
                self.record(delivery, "abandoned");
                if let Some(node) = failed {
                    context
                        .span()
                        .set_attribute(KeyValue::new("expl.verification.failed_node.id", node.0));
                }
                telemetry::error(&context, "abandoned");
            }
            ProcessActivity::Followed(_) | ProcessActivity::Reacting(_) => {}
        }
        self.observer.observe(activity);
    }
}

fn event_kind(delivery: &Delivery) -> KeyValue {
    KeyValue::new("expl.event.kind", delivery.event.kind.as_str())
}

fn record_report(report: &Report) -> &'static str {
    let context = Context::current();
    let span = context.span();
    if !span.is_recording() {
        return report_outcome(report);
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
    report_outcome(report)
}

fn report_outcome(report: &Report) -> &'static str {
    match report {
        Report::Ignored { .. } => "ignored",
        Report::Verification(report) if report.status == NodeStatus::Failed => {
            "verification_failed"
        }
        Report::Exploration { .. }
        | Report::Planning(_)
        | Report::Verification(_)
        | Report::Evaluation(_) => "acknowledged",
    }
}
