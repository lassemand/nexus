//! The live insider publish continues the item's trace (NEX-279).
//!
//! Its own test binary: tracing caches callsite interest process-wide, so a span
//! assertion racing the library's other tests (which run without a subscriber)
//! can miss spans. One test per process makes it deterministic.

use async_trait::async_trait;
use backon::ExponentialBuilder;
use chronicle_lib::nordic::events::trace_headers;
use chronicle_lib::nordic::insiders::InsiderSink;
use chronicle_lib::nordic::live::{handle_message_full, Companies, Handling, Registered, SeenSet};
use chronicle_lib::nordic::live_insiders::{run_insider_producer, InsiderQueue};
use chrono::Utc;
use model::generated::UnifiedInsiderTransaction;
use rdkafka::message::Headers;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use telemetry::trace::testing::Harness;

const INSIDERS: &str = include_str!("fixtures/mfn/finnish/news_metso_insiders.json");
const CEO_EN: &str = "9954de80-88dd-5084-a03b-4b42e17c1644";

type Sent = Vec<(String, Vec<(String, String)>)>;

#[derive(Default)]
struct HeaderSink {
    sent: Mutex<Sent>,
}

#[async_trait]
impl InsiderSink for HeaderSink {
    async fn send(&self, key: &str, _txn: &UnifiedInsiderTransaction) -> anyhow::Result<()> {
        let headers = trace_headers()
            .iter()
            .map(|h| {
                (
                    h.key.to_string(),
                    String::from_utf8(h.value.unwrap_or_default().to_vec()).unwrap(),
                )
            })
            .collect();
        self.sent.lock().unwrap().push((key.to_string(), headers));
        Ok(())
    }
}

#[tokio::test]
async fn the_insider_publish_is_a_child_of_the_items_trace_and_carries_traceparent() {
    let harness = Harness::new();
    let _guard = tracing::subscriber::set_default(harness.subscriber());
    let items: Vec<serde_json::Value> = serde_json::from_str(INSIDERS).unwrap();
    let item = items.into_iter().find(|i| i["news_id"] == CEO_EN).unwrap();
    let message = serde_json::json!({ "items": [item] }).to_string();
    let companies = Companies::new(vec![Registered {
        ticker: "METSO".into(),
        exchange_mic: "XHEL".into(),
        mfn_slug: "metso-2".into(),
        name: Some("Metso".into()),
    }]);
    let (queue, jobs) = InsiderQueue::channel(4);
    let handling = Handling {
        insiders: Some(&queue),
        ..Handling::default()
    };
    handle_message_full(
        &message,
        Utc::now(),
        &companies,
        &mut SeenSet::new(10),
        &handling,
    )
    .await;
    drop(queue);
    let sink = Arc::new(HeaderSink::default());
    let backoff = ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(1))
        .with_total_delay(Some(Duration::from_millis(10)));
    run_insider_producer(sink.clone(), jobs, backoff).await;

    let root = harness.span("nordic_live.release");
    for name in [
        "nordic_live.insider_queue",
        "nordic_live.insider_parse",
        "send insider.transactions",
    ] {
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
    let send = harness.span("send insider.transactions");
    let sent = sink.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "FIN_MAR:METSO:SAMI TAKALUOMA:2026-03-19");
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
