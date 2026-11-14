//! The spans of the release path, against an in-memory exporter: one trace from
//! `nordic-live`'s publish through the decision and out on `analyst.decisions`.

mod common;

use analyst::{prepare, Prepared, Processed, Receipt};
use common::rig::{at, trapping_kernel, Rig};
use model::generated::SpecialEvent;
use prost::Message;
use rdkafka::message::{Header, OwnedHeaders, OwnedMessage};
use rdkafka::Timestamp;
use sqlx::PgPool;
use telemetry::trace::opentelemetry::trace::{SpanId, Status, TraceContextExt};
use telemetry::trace::opentelemetry::KeyValue;
use telemetry::trace::testing::Harness;
use tracing::instrument::WithSubscriber;
use tracing::Instrument;

const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT_ID: &str = "00f067aa0ba902b7";

fn report() -> SpecialEvent {
    SpecialEvent {
        ticker: "AAA".into(),
        event_type: "quarterly_report".into(),
        occurred_at_unix_secs: at(7, 0, 0).timestamp(),
        description: "AAA: Interim report".into(),
        exchange_mic: "XSTO".into(),
        source: "mfn".into(),
        source_id: "6f9619ff-8b86-d011-b42d-00cf4fc964ff".into(),
        lang: "en".into(),
        report_kind: "interim".into(),
        fiscal_period: "2026-Q3".into(),
        text: "AAA: Interim report\n\nNet sales were 5 (4).".into(),
        ..SpecialEvent::default()
    }
}

fn message(event: &SpecialEvent, traceparent: Option<&str>) -> OwnedMessage {
    let headers = traceparent.map(|value| {
        OwnedHeaders::new().insert(Header {
            key: "traceparent",
            value: Some(value),
        })
    });
    OwnedMessage::new(
        Some(event.encode_to_vec()),
        Some(b"AAA:XSTO".to_vec()),
        "special.events".to_owned(),
        Timestamp::NotAvailable,
        0,
        42,
        headers,
    )
}

fn attribute(attributes: &[KeyValue], key: &str) -> Option<String> {
    attributes
        .iter()
        .find(|attribute| attribute.key.as_str() == key)
        .map(|attribute| attribute.value.to_string())
}

#[sqlx::test]
async fn a_release_is_one_trace_from_the_publish_to_the_decision_event(pool: PgPool) {
    let harness = Harness::new();
    let rig = Rig::new(pool);
    rig.ready_target("AAA", "2026-Q3", "2026-11-10").await;
    let traceparent = format!("00-{TRACE_ID}-{PARENT_ID}-01");
    let message = message(&report(), Some(&traceparent));

    let processed = async {
        let Prepared::Process(input, span) = prepare(&message) else {
            panic!("the report passes the filter");
        };
        let receipt = Receipt::now(rig.clock.as_ref());
        rig.processor
            .process(*input, receipt)
            .instrument(span)
            .await
    }
    .with_subscriber(harness.subscriber())
    .await;
    assert!(matches!(processed, Processed::Decided { .. }));

    let spans = harness.spans();
    let names: Vec<&str> = spans.iter().map(|span| span.name.as_ref()).collect();
    for expected in [
        "process special.events",
        "analyst.match_target",
        "analyst.kernel_run",
        "analyst.hand_off",
        "analyst.decision_write",
        "send analyst.decisions",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    assert!(spans
        .iter()
        .all(|span| span.span_context.trace_id().to_string() == TRACE_ID));
    let consumer = harness.span("process special.events");
    assert_eq!(consumer.parent_span_id.to_string(), PARENT_ID);
    assert_eq!(
        attribute(&consumer.attributes, "mfn.news_id").as_deref(),
        Some("6f9619ff-8b86-d011-b42d-00cf4fc964ff")
    );
    assert_eq!(
        attribute(&consumer.attributes, "nexus.ticker").as_deref(),
        Some("AAA")
    );
    assert_eq!(
        attribute(&consumer.attributes, "nexus.trace_parent_missing"),
        None
    );
    assert!(attribute(&consumer.attributes, "nexus.decision").is_some());
    for child in [
        "analyst.match_target",
        "analyst.kernel_run",
        "analyst.hand_off",
        "analyst.decision_write",
        "send analyst.decisions",
    ] {
        assert_eq!(
            harness.span(child).parent_span_id,
            consumer.span_context.span_id(),
            "{child} is a child of the consumer span"
        );
    }
    assert_eq!(
        attribute(
            &harness.span("analyst.decision_write").attributes,
            "analyst.write_attempts"
        )
        .as_deref(),
        Some("1")
    );

    let stored: (Option<String>,) = sqlx::query_as("SELECT trace_id FROM decisions")
        .fetch_one(&rig.pool)
        .await
        .expect("the decision");
    assert_eq!(stored.0.as_deref(), Some(TRACE_ID));
    let emitted = rig.sink.emitted();
    assert_eq!(emitted[0].1.trace_id, TRACE_ID);

    let headers = rig.sink.headers.lock().expect("sink lock").clone();
    let sent = harness.span("send analyst.decisions");
    let traceparent = headers[0]
        .iter()
        .find(|(key, _)| key == "traceparent")
        .map(|(_, value)| value.clone())
        .expect("the decision event carries traceparent");
    assert_eq!(
        traceparent,
        format!("00-{TRACE_ID}-{}-01", sent.span_context.span_id())
    );

    let signals = rig.hand_off.signals();
    let carried = signals[0].context.span().span_context().clone();
    assert!(carried.is_valid());
    assert_eq!(carried.trace_id().to_string(), TRACE_ID);
    assert_eq!(
        carried.span_id(),
        harness.span("analyst.hand_off").span_context.span_id()
    );
}

#[sqlx::test]
async fn a_release_without_a_parent_starts_a_marked_root_trace(pool: PgPool) {
    let harness = Harness::new();
    let rig = Rig::new(pool);
    rig.ready_target("AAA", "2026-Q3", "2026-11-10").await;
    let message = message(&report(), None);
    let processed = async {
        let Prepared::Process(input, span) = prepare(&message) else {
            panic!("the report passes the filter");
        };
        let receipt = Receipt::now(rig.clock.as_ref());
        rig.processor
            .process(*input, receipt)
            .instrument(span)
            .await
    }
    .with_subscriber(harness.subscriber())
    .await;
    assert!(matches!(processed, Processed::Decided { .. }));
    let consumer = harness.span("process special.events");
    assert_eq!(consumer.parent_span_id, SpanId::INVALID);
    assert_eq!(
        attribute(&consumer.attributes, "nexus.trace_parent_missing").as_deref(),
        Some("true")
    );
    let stored: (Option<String>,) = sqlx::query_as("SELECT trace_id FROM decisions")
        .fetch_one(&rig.pool)
        .await
        .expect("the decision");
    assert_eq!(
        stored.0.as_deref(),
        Some(consumer.span_context.trace_id().to_string().as_str())
    );
}

#[sqlx::test]
async fn events_that_are_not_releases_make_no_spans(pool: PgPool) {
    let harness = Harness::new();
    let _rig = Rig::new(pool);
    let mut other = report();
    other.event_type = "company_announcement".into();
    let message = message(&other, None);
    let prepared = async { prepare(&message) }
        .with_subscriber(harness.subscriber())
        .await;
    assert!(matches!(prepared, Prepared::Ignore(_)));
    assert!(harness.spans().is_empty());
}

#[sqlx::test]
async fn a_trapping_kernel_gives_an_error_span(pool: PgPool) {
    let harness = Harness::new();
    let rig = Rig::new(pool);
    let target = rig
        .target("AAA", Some("2026-Q3"), "2026-11-10", "ready")
        .await;
    rig.accept_kernel(target, &trapping_kernel()).await;
    let mut event = report();
    event.text = "Upset: Interim report".into();
    let message = message(&event, None);
    let processed = async {
        let Prepared::Process(input, span) = prepare(&message) else {
            panic!("the report passes the filter");
        };
        let receipt = Receipt::now(rig.clock.as_ref());
        rig.processor
            .process(*input, receipt)
            .instrument(span)
            .await
    }
    .with_subscriber(harness.subscriber())
    .await;
    assert!(matches!(processed, Processed::Decided { .. }));
    let run = harness.span("analyst.kernel_run");
    assert!(
        matches!(run.status, Status::Error { .. }),
        "{:?}",
        run.status
    );
}
