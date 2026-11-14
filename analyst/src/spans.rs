//! Helpers for the spans of the release path.
//!
//! Spans are made with `tracing`; `telemetry::trace::init` turns them into
//! OpenTelemetry spans and exports them in the background. Creating and closing a
//! span costs microseconds, and a collector that is down changes nothing but
//! the missing traces. See `telemetry`'s module docs for the naming and attribute
//! conventions.

use telemetry::trace::opentelemetry::trace::TraceContextExt;
use telemetry::trace::OpenTelemetrySpanExt;
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
