//! OpenTelemetry metrics and spans for workers and their deliveries.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Instant, SystemTime};

use opentelemetry::metrics::{Counter, Histogram, ObservableGauge};
use opentelemetry::trace::TraceContextExt;
use opentelemetry::{Context, ContextGuard, KeyValue, global};

use crate::delivery::{Delivery, Inbox};
use crate::process::ProcessRecord;
use crate::registration::ConsumerRegistration;
use crate::telemetry;
use crate::worker::{Activity, Observer};

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

/// Observes a worker for telemetry, then hands each activity on.
pub(super) struct WorkerTelemetry<'a> {
    observer: &'a dyn Observer,
    /// The consumer and what its registration says about it: on every
    /// metric and span.
    labels: Vec<KeyValue>,
    process: Vec<KeyValue>,
    phase: Arc<AtomicU8>,
    _state: ObservableGauge<u64>,
    deliveries: Counter<u64>,
    duration: Histogram<f64>,
    queue_age: Histogram<f64>,
}

impl<'a> WorkerTelemetry<'a> {
    pub(super) fn new(
        observer: &'a dyn Observer,
        name: &str,
        consumer: &ConsumerRegistration,
        record: &ProcessRecord,
    ) -> Self {
        let mut labels = vec![KeyValue::new("expl.consumer", name.to_owned())];
        labels.extend(
            consumer
                .attributes
                .iter()
                .map(|(key, value)| KeyValue::new(key.clone(), value.clone())),
        );
        let mut process = labels.clone();
        process.extend([
            KeyValue::new("expl.process.id", record.id.0),
            KeyValue::new("expl.process.name", record.name.clone()),
            KeyValue::new("expl.process.detached", record.detached),
            KeyValue::new("expl.consumer.replicas", consumer.replicas as i64),
        ]);
        if let Some(limit) = consumer.concurrency {
            process.push(KeyValue::new(
                "expl.consumer.concurrency",
                limit.get() as i64,
            ));
        }
        if let Some(group) = &consumer.group {
            process.push(KeyValue::new("expl.routing.group", group.clone()));
        }
        let meter = global::meter(telemetry::SCOPE);
        let phase = Arc::new(AtomicU8::new(Phase::Starting as u8));
        let current_phase = Arc::clone(&phase);
        let states = PHASES.map(|(phase, name)| {
            let mut attributes = labels.clone();
            attributes.push(KeyValue::new("expl.worker.phase", name));
            (phase as u8, attributes)
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

    /// Start the span of one delivery, linked to the span that produced its
    /// event, and make it current until the guard is dropped.
    pub(super) fn delivery(&self, inbox: &Inbox, delivery: &Delivery) -> ContextGuard {
        let mut attributes = self.delivery_attributes(delivery);
        if let Some(age) = humantime::parse_rfc3339(&delivery.event.created_at)
            .ok()
            .and_then(|time| SystemTime::now().duration_since(time).ok())
        {
            let seconds = age.as_secs_f64();
            self.queue_age
                .record(seconds, &self.measured(delivery, None));
            attributes.push(KeyValue::new("expl.delivery.queue_age_seconds", seconds));
        }
        attributes.push(KeyValue::new("expl.delivery.outcome", "unsettled"));
        // A telemetry lookup must never affect delivery of the event itself.
        let cause = inbox
            .trace_context(delivery.event.id)
            .and_then(|(parent, state)| telemetry::remote_span(&parent, state.as_deref()));
        telemetry::consumer_span(attributes, cause)
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
        if let Some(subject) = delivery.event.subject {
            attributes.push(KeyValue::new("expl.event.subject", subject));
        }
        attributes
    }

    /// The attributes of a measurement about `delivery`.
    fn measured(&self, delivery: &Delivery, outcome: Option<&str>) -> Vec<KeyValue> {
        let mut attributes = self.labels.clone();
        attributes.push(event_kind(delivery));
        if let Some(outcome) = outcome {
            attributes.push(KeyValue::new("expl.delivery.outcome", outcome.to_owned()));
        }
        attributes
    }

    fn record(&self, delivery: &Delivery, outcome: &str) {
        let attributes = self.measured(delivery, Some(outcome));
        self.deliveries.add(1, &attributes);
        let context = Context::current();
        if let Some(start) = context.get::<DeliveryStart>() {
            self.duration
                .record(start.0.elapsed().as_secs_f64(), &attributes);
        }
        context
            .span()
            .set_attribute(KeyValue::new("expl.delivery.outcome", outcome.to_owned()));
    }
}

impl Observer for WorkerTelemetry<'_> {
    fn observe(&self, activity: &Activity<'_>) {
        match activity {
            Activity::Started(_) => {
                telemetry::span("expl.worker.started", self.process.clone())
                    .span()
                    .end();
            }
            Activity::Exiting(reason) => {
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
            Activity::Reacted { delivery, kind, .. } => {
                // Abandonment can acknowledge work settled elsewhere, outside
                // the loop that normally owns the span for this delivery.
                let _scope = (Context::current().get::<DeliveryStart>().is_none()).then(|| {
                    telemetry::consumer_span(self.delivery_attributes(delivery), None).attach()
                });
                self.record(delivery, kind.unwrap_or("acknowledged"));
            }
            Activity::Failed {
                delivery, retry, ..
            } => {
                self.record(delivery, if *retry { "retry" } else { "failed" });
                let context = Context::current();
                context
                    .span()
                    .set_attribute(KeyValue::new("expl.delivery.retry", *retry));
                telemetry::error(&context, "reaction");
            }
            Activity::LeaseLost(delivery) => {
                self.record(delivery, "lease_lost");
                telemetry::error(&Context::current(), "lease_lost");
            }
            Activity::Abandoned { delivery, .. } => {
                let context = telemetry::consumer_span(self.delivery_attributes(delivery), None);
                let _scope = context.clone().attach();
                self.record(delivery, "abandoned");
                telemetry::error(&context, "abandoned");
            }
            Activity::Task { .. } | Activity::Reacting(_) => {}
        }
        self.observer.observe(activity);
    }
}

fn event_kind(delivery: &Delivery) -> KeyValue {
    KeyValue::new("expl.event.kind", delivery.event.kind.clone())
}
