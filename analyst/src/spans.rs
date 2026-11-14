//! Helpers for the spans of the release path.
//!
//! Spans are made with `tracing`; `telemetry::trace::init` turns them into
//! OpenTelemetry spans and exports them in the background. Creating and closing a
//! span costs microseconds, and a collector that is down changes nothing but
//! the missing traces. See `telemetry`'s module docs for the naming and attribute
//! conventions.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use telemetry::trace::opentelemetry::trace::{
    SpanContext, SpanId, Status, TraceContextExt, TraceFlags, TraceId, TraceState,
};
use telemetry::trace::opentelemetry::{Context, KeyValue};
use telemetry::trace::{span_from, OpenTelemetrySpanExt, END_TIME_ATTRIBUTE};
use tracing::Span;

/// The trace id of `span` as 32 lowercase hex digits, or `None` when the span has
/// no OpenTelemetry context (it is disabled, or no tracing is installed).
pub fn trace_id_of(span: &Span) -> Option<String> {
    let context = span.context();
    let span_context = context.span().span_context().clone();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
}

/// Marks `span` as failed with `kind` and logs the failure inside it, which the
/// OpenTelemetry layer records as an `exception` event. The span must have
/// declared `otel.status_code` and `otel.status_description` as empty fields.
pub fn record_failure(span: &Span, kind: &str, description: &str) {
    span.record("otel.status_code", "ERROR");
    span.record("otel.status_description", kind);
    span.in_scope(|| tracing::error!(error = %description));
}

/// `at` as a [`SystemTime`].
pub fn system_time(at: DateTime<Utc>) -> SystemTime {
    let nanos = at.timestamp_nanos_opt().unwrap_or(0);
    UNIX_EPOCH + Duration::from_nanos(u64::try_from(nanos).unwrap_or(0))
}

/// A span that starts at `start` on the analyst's clock (not when it is created)
/// and is a child of `parent`, or a new root without one. End it with
/// [`end_span`]; it is never entered, so a trade's spans do not depend on which
/// task is running.
pub fn clock_span(
    name: &str,
    start: DateTime<Utc>,
    parent: Option<&Context>,
    attributes: Vec<KeyValue>,
) -> Span {
    span_from(name, system_time(start), parent, attributes)
}

/// Sets when `span` ends on the analyst's clock; the end is applied when the
/// span closes (when its last handle is dropped).
pub fn end_span(span: &Span, at: DateTime<Utc>) {
    span.set_attribute(
        END_TIME_ATTRIBUTE,
        at.timestamp_nanos_opt().unwrap_or(i64::MAX),
    );
}

/// Marks `span` as failed with `description`.
pub fn fail_span(span: &Span, description: &str) {
    span.set_status(Status::error(description.to_owned()));
}

/// The trace id of `context` as 32 lowercase hex digits, `None` when it holds no
/// valid span.
pub fn context_trace_id(context: &Context) -> Option<String> {
    let span_context = context.span().span_context().clone();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
}

/// The W3C `traceparent` of the span in `context`, `None` when it holds none.
pub fn traceparent_of(context: &Context) -> Option<String> {
    let span_context = context.span().span_context().clone();
    span_context.is_valid().then(|| {
        format!(
            "00-{}-{}-{:02x}",
            span_context.trace_id(),
            span_context.span_id(),
            span_context.trace_flags().to_u8()
        )
    })
}

/// The remote span context of a stored W3C `traceparent`; `None` when it is not
/// a valid version `00` header.
pub fn span_context_from_traceparent(traceparent: &str) -> Option<SpanContext> {
    let mut parts = traceparent.split('-');
    let (version, trace, span, flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if version != "00" || parts.next().is_some() || trace.len() != 32 || span.len() != 16 {
        return None;
    }
    let span_context = SpanContext::new(
        TraceId::from_hex(trace).ok()?,
        SpanId::from_hex(span).ok()?,
        TraceFlags::new(u8::from_str_radix(flags, 16).ok()?),
        true,
        TraceState::default(),
    );
    span_context.is_valid().then_some(span_context)
}

/// A context whose current span is the remote span of a stored `traceparent`,
/// to use as the parent of new spans.
pub fn context_from_traceparent(traceparent: &str) -> Option<Context> {
    span_context_from_traceparent(traceparent)
        .map(|span_context| Context::new().with_remote_span_context(span_context))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_traceparent_survives_a_round_trip_and_a_bad_one_is_refused() {
        let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let context = context_from_traceparent(header).expect("valid");
        assert_eq!(traceparent_of(&context).as_deref(), Some(header));
        assert_eq!(
            context_trace_id(&context).as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        for bad in [
            "",
            "garbage",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x",
        ] {
            assert!(context_from_traceparent(bad).is_none(), "{bad}");
        }
        assert_eq!(traceparent_of(&Context::new()), None);
    }
}
