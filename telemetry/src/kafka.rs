//! W3C trace context in Kafka message headers (feature `kafka`).
//!
//! Producers add `traceparent` (and `tracestate` when it is not empty) to a record's
//! headers with [`inject_current`] or [`inject`]; consumers read them back with
//! [`extract`] and make the result the parent of their `process <topic>` span:
//!
//! ```ignore
//! let span = tracing::info_span!("process special.events", otel.kind = "consumer");
//! if let Some(cx) = telemetry::kafka::extract(&message) {
//!     let _ = span.set_parent(cx);
//! }
//! ```
//!
//! The W3C `TraceContextPropagator` is used directly, so the helpers work whether or
//! not [`crate::trace::init`] registered it globally. A missing, malformed or
//! non-UTF-8 header gives `None` and a `debug` log; it never fails the message.

use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::TraceContextExt;
use opentelemetry::Context;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use rdkafka::message::{Header, Headers, Message, OwnedHeaders};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Header carrying the W3C `traceparent`.
pub const TRACEPARENT: &str = "traceparent";

/// Header carrying the W3C `tracestate`.
pub const TRACESTATE: &str = "tracestate";

struct HeaderInjector(Vec<(&'static str, String)>);

impl Injector for HeaderInjector {
    fn set(&mut self, key: &str, value: String) {
        let key = match key {
            TRACEPARENT => TRACEPARENT,
            TRACESTATE if !value.is_empty() => TRACESTATE,
            _ => return,
        };
        self.0.push((key, value));
    }
}

struct HeaderExtractor<'a, H: Headers>(&'a H);

impl<H: Headers> Extractor for HeaderExtractor<'_, H> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|header| header.key.eq_ignore_ascii_case(key))
            .and_then(|header| header.value)
            .and_then(|value| std::str::from_utf8(value).ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.iter().map(|header| header.key).collect()
    }
}

/// Adds the trace context of `cx` to `headers` (`traceparent`, and `tracestate` when
/// not empty). Leaves `headers` unchanged when `cx` has no valid span.
pub fn inject(cx: &Context, headers: OwnedHeaders) -> OwnedHeaders {
    let mut injector = HeaderInjector(Vec::with_capacity(2));
    TraceContextPropagator::new().inject_context(cx, &mut injector);
    injector.0.iter().fold(headers, |headers, (key, value)| {
        headers.insert(Header {
            key,
            value: Some(value.as_str()),
        })
    })
}

/// [`inject`] with the context of the current `tracing` span.
pub fn inject_current(headers: OwnedHeaders) -> OwnedHeaders {
    inject(&tracing::Span::current().context(), headers)
}

/// Reads the trace context from a message's headers; `None` when `traceparent` is
/// absent, malformed or not UTF-8.
pub fn extract<M: Message>(message: &M) -> Option<Context> {
    let Some(headers) = message.headers() else {
        tracing::debug!(
            topic = message.topic(),
            "kafka message without headers, no trace context"
        );
        return None;
    };
    let extractor = HeaderExtractor(headers);
    let Some(traceparent) = extractor.get(TRACEPARENT) else {
        tracing::debug!(
            topic = message.topic(),
            "kafka message without a UTF-8 traceparent header"
        );
        return None;
    };
    let cx = TraceContextPropagator::new().extract(&extractor);
    if cx.span().span_context().is_valid() {
        Some(cx)
    } else {
        tracing::debug!(
            topic = message.topic(),
            traceparent,
            "malformed traceparent header"
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::TraceContextExt;
    use rdkafka::message::{OwnedMessage, Timestamp};

    use super::*;
    use crate::trace::testing::Harness;

    fn message(headers: Option<OwnedHeaders>) -> OwnedMessage {
        OwnedMessage::new(
            Some(b"payload".to_vec()),
            Some(b"GOMX:XSTO".to_vec()),
            "special.events".to_string(),
            Timestamp::NotAvailable,
            0,
            42,
            headers,
        )
    }

    fn header_value<'a>(headers: &'a OwnedHeaders, key: &str) -> Option<&'a [u8]> {
        headers
            .iter()
            .find(|header| header.key == key)
            .and_then(|header| header.value)
    }

    #[test]
    fn round_trip_continues_the_trace_in_another_service() {
        let producer = Harness::new();
        let headers = tracing::subscriber::with_default(producer.subscriber(), || {
            tracing::info_span!("send special.events", otel.kind = "producer").in_scope(|| {
                inject_current(OwnedHeaders::new().insert(Header {
                    key: "content-type",
                    value: Some("application/protobuf"),
                }))
            })
        });
        assert!(header_value(&headers, TRACEPARENT).is_some());
        assert!(header_value(&headers, TRACESTATE).is_none());
        assert_eq!(
            header_value(&headers, "content-type"),
            Some(&b"application/protobuf"[..])
        );
        let message = message(Some(headers));

        let consumer = Harness::new();
        tracing::subscriber::with_default(consumer.subscriber(), || {
            let cx = extract(&message).expect("trace context in headers");
            assert!(cx.span().span_context().is_remote());
            let span =
                tracing::info_span!(parent: None, "process special.events", otel.kind = "consumer");
            span.set_parent(cx)
                .expect("parent set before the span starts");
            span.in_scope(|| tracing::info_span!("analyst.kernel_run").in_scope(|| {}));
        });

        let send = producer.span("send special.events");
        let process = consumer.span("process special.events");
        let kernel = consumer.span("analyst.kernel_run");
        assert_eq!(
            process.span_context.trace_id(),
            send.span_context.trace_id()
        );
        assert_eq!(process.parent_span_id, send.span_context.span_id());
        assert_eq!(kernel.span_context.trace_id(), send.span_context.trace_id());
        assert_eq!(kernel.parent_span_id, process.span_context.span_id());
    }

    #[test]
    fn inject_without_a_span_adds_nothing() {
        let headers = inject(&Context::new(), OwnedHeaders::new());
        assert_eq!(headers.count(), 0);
    }

    #[test]
    fn tracestate_is_carried_when_present() {
        use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};
        let state = TraceState::from_key_value([("nexus", "1")]).expect("valid tracestate");
        let span_context = SpanContext::new(
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").expect("hex"),
            SpanId::from_hex("00f067aa0ba902b7").expect("hex"),
            TraceFlags::SAMPLED,
            true,
            state,
        );
        let cx = Context::new().with_remote_span_context(span_context.clone());
        let headers = inject(&cx, OwnedHeaders::new());
        assert_eq!(
            header_value(&headers, TRACEPARENT),
            Some(&b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"[..])
        );
        assert_eq!(header_value(&headers, TRACESTATE), Some(&b"nexus=1"[..]));
        let extracted = extract(&message(Some(headers))).expect("context");
        assert_eq!(extracted.span().span_context(), &span_context);
    }

    #[test]
    fn missing_headers_give_none() {
        assert!(extract(&message(None)).is_none());
        assert!(extract(&message(Some(OwnedHeaders::new()))).is_none());
        let unrelated = OwnedHeaders::new().insert(Header {
            key: "content-type",
            value: Some("x"),
        });
        assert!(extract(&message(Some(unrelated))).is_none());
        let empty_value = OwnedHeaders::new().insert::<str>(Header {
            key: TRACEPARENT,
            value: None,
        });
        assert!(extract(&message(Some(empty_value))).is_none());
    }

    #[test]
    fn malformed_traceparent_gives_none() {
        for value in [
            "",
            "garbage",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "zz-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ] {
            let headers = OwnedHeaders::new().insert(Header {
                key: TRACEPARENT,
                value: Some(value),
            });
            assert!(extract(&message(Some(headers))).is_none(), "{value:?}");
        }
    }

    #[test]
    fn non_utf8_traceparent_gives_none() {
        let bytes: &[u8] = &[0x30, 0x30, 0x2d, 0xff, 0xfe, 0xfd];
        let headers = OwnedHeaders::new().insert(Header {
            key: TRACEPARENT,
            value: Some(bytes),
        });
        assert!(extract(&message(Some(headers))).is_none());
    }
}
