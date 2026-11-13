//! The latency of a decision with tracing on and the collector unreachable. It
//! is its own test binary because the tracing subscriber is installed globally.

mod common;

use std::time::Duration;

use analyst::{prepare, Prepared, Processed, Receipt};
use chrono::Duration as ChronoDuration;
use common::rig::{at, Rig};
use model::generated::SpecialEvent;
use prost::Message;
use rdkafka::message::OwnedMessage;
use rdkafka::Timestamp;
use sqlx::PgPool;
use tracing::Instrument;

fn release(ticker: &str, id: usize) -> OwnedMessage {
    let event = SpecialEvent {
        ticker: ticker.into(),
        event_type: "quarterly_report".into(),
        occurred_at_unix_secs: (at(7, 0, 0) + ChronoDuration::days(60 + id as i64)).timestamp(),
        description: format!("{ticker}: Interim report"),
        exchange_mic: "XSTO".into(),
        source: "mfn".into(),
        source_id: format!("{ticker}-{id}"),
        lang: "en".into(),
        report_kind: "interim".into(),
        fiscal_period: format!("2027-Q{}", id % 4 + 1),
        text: format!("{ticker}: Interim report\n\nNet sales were 5 (4)."),
        ..SpecialEvent::default()
    };
    OwnedMessage::new(
        Some(event.encode_to_vec()),
        Some(format!("{ticker}:XSTO").into_bytes()),
        "special.events".to_owned(),
        Timestamp::NotAvailable,
        0,
        id as i64,
        None,
    )
}

#[sqlx::test]
async fn the_latency_with_tracing_on_and_a_dead_collector_stays_under_the_budget(pool: PgPool) {
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:1");
    let _guard = telemetry::trace::init("analyst-latency-test").expect("tracing starts");
    let rig = Rig::new(pool);
    let companies: Vec<String> = (0..20).map(|i| format!("C{i:02}")).collect();
    for ticker in &companies {
        rig.ready_target(ticker, "2027-Q1", "2027-01-10").await;
    }
    let mut times = Vec::new();
    for ticker in &companies {
        for id in 0..10 {
            let message = release(ticker, id);
            let Prepared::Process(input, span) = prepare(&message) else {
                panic!("the report passes the filter");
            };
            rig.clock.set(at(7, 0, 5));
            let receipt = Receipt::now(rig.clock.as_ref());
            match rig
                .processor
                .process(*input, receipt)
                .instrument(span)
                .await
            {
                Processed::Decided { to_hand_off, .. } => times.push(to_hand_off),
                other => panic!("expected a decision, got {other:?}"),
            }
        }
    }
    times.sort();
    let p50 = times[times.len() / 2];
    let p99 = times[times.len() * 99 / 100];
    eprintln!(
        "traced, collector down, {} releases: p50 {p50:?} p99 {p99:?} max {:?}",
        times.len(),
        times[times.len() - 1]
    );
    assert!(p99 < Duration::from_millis(10), "p99 was {p99:?}");
}
