//! Telemetry shared by the runtime and the explorers built on it.

use std::str::FromStr;

use opentelemetry::trace::{
    Link, SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags, TraceId, TraceState,
    Tracer,
};
use opentelemetry::{Context, KeyValue, global};

/// The instrumentation scope of every span and metric.
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

/// Mark the span of `context` as failed for the reason `kind`.
pub fn error(context: &Context, kind: &'static str) {
    let span = context.span();
    span.set_attribute(KeyValue::new("error.type", kind));
    span.set_status(Status::error(kind));
}

/// The span of one delivery. It starts its own trace, so a continuous
/// exploration cannot grow a trace indefinitely; a link retains the span
/// that produced the event across the durable inbox.
#[must_use]
pub fn consumer_span(attributes: Vec<KeyValue>, cause: Option<SpanContext>) -> Context {
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

/// The span of `context` as W3C trace context (`traceparent`, and
/// `tracestate` when it has one), to hand to another process. `None` when
/// the context has no valid span.
#[must_use]
pub fn trace_context(context: &Context) -> Option<(String, Option<String>)> {
    let span = context.span();
    let span = span.span_context();
    if !span.is_valid() {
        return None;
    }
    let parent = format!(
        "00-{}-{}-{:02x}",
        span.trace_id(),
        span.span_id(),
        span.trace_flags().to_u8()
    );
    let state = span.trace_state().header();
    Some((parent, (!state.is_empty()).then_some(state)))
}

/// The span another process described as W3C trace context. `None` when
/// `traceparent` is not a valid one.
#[must_use]
pub fn remote_span(traceparent: &str, tracestate: Option<&str>) -> Option<SpanContext> {
    let mut parts = traceparent.trim().split('-');
    let (version, trace, span, flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if version.len() != 2 || trace.len() != 32 || span.len() != 16 || flags.len() != 2 {
        return None;
    }
    let context = SpanContext::new(
        TraceId::from_hex(trace).ok()?,
        SpanId::from_hex(span).ok()?,
        TraceFlags::new(u8::from_str_radix(flags, 16).ok()?),
        true,
        tracestate
            .and_then(|state| TraceState::from_str(state).ok())
            .unwrap_or_default(),
    );
    context.is_valid().then_some(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_survives_the_trip_to_another_process() {
        let parent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        let span = remote_span(parent, Some("vendor=value")).expect("span");
        assert!(span.is_remote() && span.is_sampled());
        let context = Context::new().with_remote_span_context(span);
        assert_eq!(
            trace_context(&context),
            Some((parent.to_owned(), Some("vendor=value".to_owned())))
        );
        for invalid in ["", "00-abc-def-01", "00-0-0-00", "nonsense"] {
            assert!(remote_span(invalid, None).is_none(), "{invalid}");
        }
        assert_eq!(trace_context(&Context::new()), None);
    }
}
