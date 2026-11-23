//! Shared OpenTelemetry instrumentation. A host can install SDK providers
//! before using `expl_core`. The default providers leave instrumentation inert.

use std::collections::HashMap;

use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::{Array, Context, KeyValue, Value};
use opentelemetry_sdk::propagation::TraceContextPropagator;

use crate::graph::{Node, NodeId};

pub(crate) use expl_runtime::telemetry::{error, span};

pub(crate) fn inject() -> HashMap<String, String> {
    let mut headers = HashMap::new();
    TraceContextPropagator::new().inject_context(&Context::current(), &mut headers);
    headers
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
