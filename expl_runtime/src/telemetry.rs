//! Telemetry shared by the runtime and exploration.

use opentelemetry::trace::{Status, TraceContextExt, Tracer};
use opentelemetry::{Context, KeyValue, global};

pub const SCOPE: &str = "expl";

/// Start a span in the current context.
#[must_use]
pub fn span(name: &'static str, attributes: Vec<KeyValue>) -> Context {
    let tracer = global::tracer(SCOPE);
    let parent = Context::current();
    let span = tracer
        .span_builder(name)
        .with_attributes(attributes)
        .start_with_context(&tracer, &parent);
    parent.with_span(span)
}

pub fn error(context: &Context, kind: &'static str) {
    let span = context.span();
    span.set_attribute(KeyValue::new("error.type", kind));
    span.set_status(Status::error(kind));
}
