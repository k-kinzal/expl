//! Shared OpenTelemetry instrumentation. A host can install SDK providers
//! before using `expl_core`. The default providers leave instrumentation inert.

use std::collections::HashMap;

use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{Link, SpanContext, SpanKind, Status, TraceContextExt, Tracer};
use opentelemetry::{Array, Context, KeyValue, Value, global};
use opentelemetry_sdk::propagation::TraceContextPropagator;

use crate::graph::{Node, NodeId};

pub(crate) const SCOPE: &str = "expl";

pub(crate) fn span(name: &'static str, attributes: Vec<KeyValue>) -> Context {
    let tracer = global::tracer(SCOPE);
    let parent = Context::current();
    let span = tracer
        .span_builder(name)
        .with_attributes(attributes)
        .start_with_context(&tracer, &parent);
    parent.with_span(span)
}

// A delivery starts its own trace so a continuous exploration cannot grow a
// trace indefinitely. A link retains the cause across the durable inbox.
pub(crate) fn consumer_span(attributes: Vec<KeyValue>, cause: Option<SpanContext>) -> Context {
    let tracer = global::tracer(SCOPE);
    let root = Context::new();
    let mut builder = tracer
        .span_builder("expl.delivery")
        .with_kind(SpanKind::Consumer)
        .with_attributes(attributes);
    if let Some(cause) = cause {
        builder = builder.with_links(vec![Link::new(cause, Vec::new(), 0)]);
    }
    root.with_span(builder.start_with_context(&tracer, &root))
}

pub(crate) fn inject() -> HashMap<String, String> {
    let mut headers = HashMap::new();
    TraceContextPropagator::new().inject_context(&Context::current(), &mut headers);
    headers
}

pub(crate) fn extract(parent: String, state: Option<String>) -> Option<SpanContext> {
    let mut headers = HashMap::from([("traceparent".to_owned(), parent)]);
    if let Some(state) = state {
        headers.insert("tracestate".to_owned(), state);
    }
    let context = TraceContextPropagator::new().extract_with_context(&Context::new(), &headers);
    let span = context.span();
    span.span_context()
        .is_valid()
        .then(|| span.span_context().clone())
}

pub(crate) fn node_ids(nodes: impl IntoIterator<Item = NodeId>) -> Value {
    Value::Array(Array::I64(nodes.into_iter().map(|id| id.0).collect()))
}

pub(crate) fn attributes(values: impl IntoIterator<Item = KeyValue>) {
    Context::current().span().set_attributes(values);
}

pub(crate) fn related_goals(goals: &[Node]) {
    attributes([
        KeyValue::new("expl.goal.ids", node_ids(goals.iter().map(|goal| goal.id))),
        KeyValue::new("expl.goal.count", goals.len() as i64),
    ]);
}

pub(crate) fn subject(node: &Node) {
    attributes([
        KeyValue::new("expl.node.id", node.id.0),
        KeyValue::new("expl.node.kind", node.kind.as_str()),
        KeyValue::new("expl.node.status", node.status.as_str()),
        KeyValue::new("expl.node.priority", node.priority),
        KeyValue::new("expl.node.investigation", node.investigation),
        KeyValue::new("expl.node.counter_exploration", node.counter_exploration),
    ]);
}

pub(crate) fn operation<T, E>(
    name: &'static str,
    attributes: Vec<KeyValue>,
    error_kind: &'static str,
    run: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let context = span(name, attributes);
    let _scope = context.clone().attach();
    let result = run();
    if result.is_err() {
        error(&context, error_kind);
    }
    result
}

pub(crate) fn error(context: &Context, kind: &'static str) {
    let span = context.span();
    span.set_attribute(KeyValue::new("error.type", kind));
    span.set_status(Status::error(kind));
}
