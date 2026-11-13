//! One trace per relevant item (NEX-266): spans, attributes and the trace context
//! on the produced message, against telemetry's in-memory exporter.

use super::events::{trace_headers, EventSink, SPECIAL_EVENTS_TOPIC};
use super::feed::parse_document;
use super::live::{
    handle_catchup_items, handle_message_full, publications, publish_live, Companies, Disposition,
    Handling, IngestedCheck, Registered, SeenSet,
};
use super::live_emit::{producer_backoff, run_producer, Emitter};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use model::generated::SpecialEvent;
use rdkafka::message::Headers;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use telemetry::trace::opentelemetry::trace::{SpanKind, Status};
use telemetry::trace::opentelemetry::Value;
use telemetry::trace::testing::{Harness, SpanData};
use uuid::Uuid;

const GOMSPACE: &str = include_str!("../../tests/fixtures/mfn_ws_gomspace_report.json");

fn companies() -> Companies {
    Companies::new(vec![Registered {
        ticker: "GOMX".into(),
        exchange_mic: "FNSE".into(),
        mfn_slug: "gomspace-group".into(),
        name: None,
    }])
}

fn publish_date() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 26, 6, 0, 0).unwrap()
}

fn received(secs_after: i64) -> DateTime<Utc> {
    publish_date() + chrono::Duration::seconds(secs_after)
}

fn warning() -> String {
    let mut doc: serde_json::Value = serde_json::from_str(GOMSPACE).unwrap();
    let item = &mut doc["items"][0];
    item["properties"]["tags"] = serde_json::json!([":regulatory"]);
    item["content"]["title"] =
        serde_json::json!("GomSpace Group AB: Downward adjustment of guidance for 2026");
    item["content"]["text"] = serde_json::json!("The board lowers its outlook.");
    item["news_id"] = serde_json::json!("7e343aa5-f961-55a9-a00e-a94cf5d4d401");
    doc.to_string()
}

struct Check(bool);

#[async_trait]
impl IngestedCheck for Check {
    async fn already_ingested(&self, _news_id: Uuid) -> Result<bool, String> {
        Ok(self.0)
    }
}

type Sent = Vec<(String, Vec<(String, String)>)>;

#[derive(Default)]
struct HeaderSink {
    sent: Mutex<Sent>,
    fail: bool,
}

#[async_trait]
impl EventSink for HeaderSink {
    async fn send(&self, key: &str, _event: &SpecialEvent) -> anyhow::Result<()> {
        if self.fail {
            anyhow::bail!("broker unavailable");
        }
        let headers = trace_headers();
        let pairs = headers
            .iter()
            .map(|h| {
                (
                    h.key.to_string(),
                    String::from_utf8(h.value.unwrap_or_default().to_vec()).unwrap(),
                )
            })
            .collect();
        self.sent.lock().unwrap().push((key.to_string(), pairs));
        Ok(())
    }
}

fn attr(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|a| a.key.as_str() == key)
        .map(|a| a.value.clone())
}

fn names(harness: &Harness) -> Vec<String> {
    let mut names: Vec<String> = harness
        .spans()
        .into_iter()
        .map(|s| s.name.into_owned())
        .collect();
    names.sort();
    names
}

fn assert_children(harness: &Harness, root: &SpanData, children: &[&str]) {
    for name in children {
        let child = harness.span(name);
        assert_eq!(
            child.span_context.trace_id(),
            root.span_context.trace_id(),
            "{name} is in the item's trace"
        );
        assert_eq!(
            child.parent_span_id,
            root.span_context.span_id(),
            "{name} is a child of the root"
        );
    }
}

#[tokio::test]
async fn a_report_is_one_trace_that_starts_at_mfns_publish_time() {
    let harness = Harness::new();
    let _guard = tracing::subscriber::set_default(harness.subscriber());
    let sink = HeaderSink::default();
    let check = Check(false);
    let handling = Handling {
        check: Some(&check),
        ..Handling::default()
    };
    let handled = handle_message_full(
        GOMSPACE,
        received(2),
        &companies(),
        &mut SeenSet::new(10),
        &handling,
    )
    .await;
    let news_id = match &handled.dispositions[0] {
        Disposition::New(item) => item.news_id,
        other => panic!("unexpected {other:?}"),
    };
    let pubs = publications(&handled.dispositions);
    assert!(publish_live(&sink, &pubs[0].key, &pubs[0].event, pubs[0].span.span()).await);
    drop(pubs);
    drop(handled);

    assert_eq!(
        names(&harness),
        [
            "mfn.delivery",
            "nordic_live.ingested_check",
            "nordic_live.parse",
            "nordic_live.release",
            "send special.events"
        ]
    );
    let root = harness.span("nordic_live.release");
    assert_eq!(root.start_time, SystemTime::from(publish_date()));
    assert!(root.end_time > root.start_time);
    assert_eq!(root.span_kind, SpanKind::Internal);
    assert_eq!(
        root.parent_span_id,
        telemetry::trace::opentelemetry::trace::SpanId::INVALID
    );
    assert_children(
        &harness,
        &root,
        &[
            "mfn.delivery",
            "nordic_live.parse",
            "nordic_live.ingested_check",
            "send special.events",
        ],
    );
    let text = |key: &str| attr(&root, key).map(|v| v.to_string());
    assert_eq!(text("nexus.ticker").as_deref(), Some("GOMX"));
    assert_eq!(text("nexus.exchange_mic").as_deref(), Some("FNSE"));
    assert_eq!(text("mfn.news_id"), Some(news_id.to_string()));
    assert_eq!(
        text("nexus.event_type").as_deref(),
        Some("quarterly_report")
    );
    assert_eq!(text("nexus.lang").as_deref(), Some("en"));
    assert_eq!(text("nordic_live.path").as_deref(), Some("socket"));
    assert_eq!(
        attr(&root, "nordic_live.publish_to_receive_ms"),
        Some(Value::I64(2_000))
    );
    assert_eq!(text("nordic_live.disposition").as_deref(), Some("new"));
    assert_eq!(root.status, Status::Unset);

    let delivery = harness.span("mfn.delivery");
    assert_eq!(delivery.start_time, SystemTime::from(publish_date()));
    assert_eq!(delivery.end_time, SystemTime::from(received(2)));
    assert_eq!(
        attr(&delivery, "nexus.clock"),
        Some(Value::from("external"))
    );
    assert_eq!(attr(&delivery, "nexus.clock_skew_ms"), None);

    let parse = harness.span("nordic_live.parse");
    assert_eq!(
        attr(&parse, "nordic_live.items_in_message"),
        Some(Value::I64(1))
    );
    assert!(parse.start_time >= SystemTime::from(received(2)) - std::time::Duration::from_secs(1));

    let check_span = harness.span("nordic_live.ingested_check");
    assert_eq!(
        attr(&check_span, "nordic_live.disposition"),
        Some(Value::from("new"))
    );

    let send = harness.span("send special.events");
    assert_eq!(send.span_kind, SpanKind::Producer);
    assert_eq!(attr(&send, "messaging.system"), Some(Value::from("kafka")));
    assert_eq!(
        attr(&send, "messaging.destination.name"),
        Some(Value::from(SPECIAL_EVENTS_TOPIC))
    );
    assert_eq!(
        attr(&send, "messaging.kafka.message.key"),
        Some(Value::from("GOMX:FNSE"))
    );

    let sent = sink.sent.lock().unwrap();
    let (key, headers) = &sent[0];
    assert_eq!(key, "GOMX:FNSE");
    assert_eq!(
        headers,
        &[(
            "traceparent".to_string(),
            format!(
                "00-{}-{}-01",
                send.span_context.trace_id(),
                send.span_context.span_id()
            )
        )]
    );
}

#[tokio::test]
async fn an_item_of_an_unregistered_company_gets_no_spans() {
    let harness = Harness::new();
    let _guard = tracing::subscriber::set_default(harness.subscriber());
    let handling = Handling {
        check: Some(&Check(false)),
        ..Handling::default()
    };
    let handled = handle_message_full(
        GOMSPACE,
        received(2),
        &Companies::new(vec![]),
        &mut SeenSet::new(10),
        &handling,
    )
    .await;
    assert!(handled.dispositions.is_empty());
    drop(handled);
    assert!(harness.spans().is_empty());
}

#[tokio::test]
async fn an_item_the_poll_already_stored_ends_its_trace_without_a_send() {
    let harness = Harness::new();
    let _guard = tracing::subscriber::set_default(harness.subscriber());
    let handling = Handling {
        check: Some(&Check(true)),
        ..Handling::default()
    };
    let handled = handle_message_full(
        GOMSPACE,
        received(2),
        &companies(),
        &mut SeenSet::new(10),
        &handling,
    )
    .await;
    assert!(publications(&handled.dispositions).is_empty());
    drop(handled);

    assert_eq!(
        names(&harness),
        [
            "mfn.delivery",
            "nordic_live.ingested_check",
            "nordic_live.parse",
            "nordic_live.release"
        ]
    );
    let root = harness.span("nordic_live.release");
    assert_eq!(
        attr(&root, "nordic_live.disposition"),
        Some(Value::from("already_ingested"))
    );
    assert_eq!(
        attr(
            &harness.span("nordic_live.ingested_check"),
            "nordic_live.disposition"
        ),
        Some(Value::from("already_ingested"))
    );
}

#[tokio::test]
async fn a_catch_up_item_is_marked_with_its_path() {
    let harness = Harness::new();
    let _guard = tracing::subscriber::set_default(harness.subscriber());
    let items = parse_document(GOMSPACE).unwrap().items;
    let handled = handle_catchup_items(
        items,
        received(600),
        &companies(),
        &mut SeenSet::new(10),
        &Handling::default(),
    )
    .await;
    drop(handled);
    let root = harness.span("nordic_live.release");
    assert_eq!(
        attr(&root, "nordic_live.path"),
        Some(Value::from("catch_up"))
    );
    assert_eq!(
        attr(&root, "nordic_live.disposition"),
        Some(Value::from("new"))
    );
    assert_eq!(
        attr(&root, "nordic_live.publish_to_receive_ms"),
        Some(Value::I64(600_000))
    );
}

#[tokio::test]
async fn a_failed_publish_marks_the_send_and_the_root_as_errors() {
    let harness = Harness::new();
    let _guard = tracing::subscriber::set_default(harness.subscriber());
    let sink = HeaderSink {
        fail: true,
        ..HeaderSink::default()
    };
    let handled = handle_message_full(
        GOMSPACE,
        received(2),
        &companies(),
        &mut SeenSet::new(10),
        &Handling::default(),
    )
    .await;
    let pubs = publications(&handled.dispositions);
    assert!(!publish_live(&sink, &pubs[0].key, &pubs[0].event, pubs[0].span.span()).await);
    drop(pubs);
    drop(handled);

    let expected = Status::Error {
        description: "broker unavailable".into(),
    };
    assert_eq!(harness.span("send special.events").status, expected);
    assert_eq!(harness.span("nordic_live.release").status, expected);
    assert!(harness
        .span("send special.events")
        .events
        .iter()
        .any(|e| e.name == "exception"));
}

#[tokio::test]
async fn an_announcement_keeps_its_trace_through_the_emit_queue() {
    let harness = Harness::new();
    let _guard = tracing::subscriber::set_default(harness.subscriber());
    let sink = Arc::new(HeaderSink::default());
    let (emitter, jobs) = Emitter::channel(10);
    let handling = Handling {
        emitter: Some(&emitter),
        ..Handling::default()
    };
    let handled = handle_message_full(
        &warning(),
        received(2),
        &companies(),
        &mut SeenSet::new(10),
        &handling,
    )
    .await;
    assert!(publications(&handled.dispositions).is_empty());
    drop(handled);
    drop(emitter);
    run_producer(sink.clone(), jobs, producer_backoff()).await;

    assert_eq!(
        names(&harness),
        [
            "mfn.delivery",
            "nordic_live.emit_queue",
            "nordic_live.parse",
            "nordic_live.release",
            "send special.events"
        ]
    );
    let root = harness.span("nordic_live.release");
    assert_eq!(
        attr(&root, "nexus.event_type"),
        Some(Value::from("company_announcement"))
    );
    assert_children(
        &harness,
        &root,
        &[
            "nordic_live.emit_queue",
            "send special.events",
            "nordic_live.parse",
        ],
    );
    let queue = harness.span("nordic_live.emit_queue");
    let send = harness.span("send special.events");
    assert!(queue.end_time <= send.start_time);
    let sent = sink.sent.lock().unwrap();
    assert_eq!(
        sent[0].1,
        [(
            "traceparent".to_string(),
            format!(
                "00-{}-{}-01",
                send.span_context.trace_id(),
                send.span_context.span_id()
            )
        )]
    );
}

#[tokio::test]
async fn a_publish_time_after_the_receive_time_gives_a_zero_length_delivery() {
    let harness = Harness::new();
    let _guard = tracing::subscriber::set_default(harness.subscriber());
    let mut doc: serde_json::Value = serde_json::from_str(GOMSPACE).unwrap();
    doc["items"][0]["content"]["publish_date"] = serde_json::json!("2026-08-26T06:00:02Z");
    let handled = handle_message_full(
        &doc.to_string(),
        received(0),
        &companies(),
        &mut SeenSet::new(10),
        &Handling::default(),
    )
    .await;
    drop(handled);
    let delivery = harness.span("mfn.delivery");
    assert_eq!(delivery.start_time, delivery.end_time);
    assert_eq!(
        attr(&delivery, "nexus.clock_skew_ms"),
        Some(Value::I64(2_000))
    );
    assert_eq!(
        attr(
            &harness.span("nordic_live.release"),
            "nordic_live.publish_to_receive_ms"
        ),
        Some(Value::I64(-2_000))
    );
}

#[tokio::test]
async fn a_send_outside_any_span_carries_no_trace_headers() {
    let sink = HeaderSink::default();
    sink.send("GOMX:FNSE", &SpecialEvent::default())
        .await
        .unwrap();
    assert!(sink.sent.lock().unwrap()[0].1.is_empty());
}

#[tokio::test]
#[ignore = "benchmark: cargo test -p chronicle --release --lib -- --ignored --nocapture tracing_overhead"]
async fn tracing_overhead() {
    async fn run(n: usize) -> Vec<std::time::Duration> {
        let check = Check(false);
        let handling = Handling {
            check: Some(&check),
            ..Handling::default()
        };
        let companies = companies();
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            let started = std::time::Instant::now();
            let handled = handle_message_full(
                GOMSPACE,
                received(2),
                &companies,
                &mut SeenSet::new(10),
                &handling,
            )
            .await;
            drop(handled);
            samples.push(started.elapsed());
        }
        samples.sort();
        samples
    }
    fn show(label: &str, samples: &[std::time::Duration]) {
        println!(
            "{label}: p50 {:?}, p99 {:?}",
            samples[samples.len() / 2],
            samples[samples.len() * 99 / 100]
        );
    }

    show("no subscriber", &run(5_000).await);
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9");
    std::env::set_var("RUST_LOG", "off");
    let guard = telemetry::trace::init("bench").unwrap();
    show("exporter at a closed port", &run(5_000).await);
    drop(guard);
}
