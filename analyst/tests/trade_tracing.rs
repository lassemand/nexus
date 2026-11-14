//! The spans of a simulated trade, against an in-memory exporter: entry inside
//! the release trace, the holding period in its own trace linked to the release,
//! the producer spans, and a restart.

mod common;

use std::time::Duration;

use analyst::spans::system_time;
use analyst::{DecisionKind, StoredTrade, TradeStatus, TradeStore};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use common::trading::{dec, hand_off, quote, rig, signal, sto, Rig};
use telemetry::trace::opentelemetry::trace::{SpanKind, Status};
use telemetry::trace::opentelemetry::KeyValue;
use telemetry::trace::testing::{Harness, SpanData};
use telemetry::trace::OpenTelemetrySpanExt;

fn attribute(attributes: &[KeyValue], key: &str) -> Option<String> {
    attributes
        .iter()
        .find(|attribute| attribute.key.as_str() == key)
        .map(|attribute| attribute.value.to_string())
}

fn spans_named(harness: &Harness, name: &str) -> Vec<SpanData> {
    harness
        .spans()
        .into_iter()
        .filter(|span| span.name == name)
        .collect()
}

fn flat(rig_start: DateTime<Utc>) -> Rig {
    rig(rig_start, Duration::ZERO, 8, |_| {
        Ok(quote("101.50", "100.00"))
    })
}

async fn run(minutes: u64) {
    tokio::time::sleep(Duration::from_secs(minutes * 60)).await;
}

#[tokio::test(start_paused = true)]
async fn an_in_session_buy_is_entered_in_the_release_trace_and_held_in_its_own() {
    let harness = Harness::new();
    let _subscriber = tracing::subscriber::set_default(harness.subscriber());
    let released = sto(11, 10, 11, 0, 0);
    let rig = flat(released);
    let decision_span = tracing::info_span!("analyst.hand_off");
    let mut signal = signal(1, "AAA", "XSTO", DecisionKind::Buy, released);
    signal.context = decision_span.context();
    hand_off(&rig, signal);
    run(8 * 60).await;
    drop(decision_span);

    let decision = harness.span("analyst.hand_off");
    let release_trace = decision.span_context.trace_id();
    let entry = harness.span("analyst.trade_entry");
    assert_eq!(entry.span_context.trace_id(), release_trace);
    assert_eq!(entry.parent_span_id, decision.span_context.span_id());
    assert_eq!(entry.start_time, system_time(released));
    assert_eq!(
        attribute(&entry.attributes, "nexus.ticker").as_deref(),
        Some("AAA")
    );
    assert_eq!(
        attribute(&entry.attributes, "nexus.decision").as_deref(),
        Some("buy")
    );
    assert_eq!(
        attribute(&entry.attributes, "analyst.shadow").as_deref(),
        Some("false")
    );
    assert_eq!(
        attribute(&entry.attributes, "analyst.trade_status").as_deref(),
        Some("open")
    );
    assert_eq!(
        attribute(&entry.attributes, "analyst.release_to_entry_ms").as_deref(),
        Some("300")
    );
    assert!(attribute(&entry.attributes, "analyst.entry_target_at").is_some());

    let quotes = spans_named(&harness, "analyst.quote");
    let entry_quotes: Vec<_> = quotes
        .iter()
        .filter(|q| attribute(&q.attributes, "analyst.quote_purpose").as_deref() == Some("entry"))
        .collect();
    assert_eq!(entry_quotes.len(), 1);
    assert_eq!(entry_quotes[0].parent_span_id, entry.span_context.span_id());
    assert_eq!(
        attribute(&entry_quotes[0].attributes, "analyst.attempt").as_deref(),
        Some("1")
    );
    assert_eq!(
        attribute(&entry_quotes[0].attributes, "analyst.accepted").as_deref(),
        Some("true")
    );
    assert_eq!(
        attribute(&entry_quotes[0].attributes, "http.response.status_code").as_deref(),
        Some("200")
    );
    assert_eq!(
        attribute(&entry_quotes[0].attributes, "analyst.ask_type").as_deref(),
        Some("Tradable")
    );

    let trade = harness.span("analyst.trade");
    assert_ne!(
        trade.span_context.trace_id(),
        release_trace,
        "its own trace"
    );
    assert_eq!(
        trade.parent_span_id,
        telemetry::trace::opentelemetry::trace::SpanId::INVALID
    );
    assert_eq!(trade.start_time, system_time(released));
    let links: Vec<_> = trade.links.iter().collect();
    assert_eq!(links.len(), 1);
    assert_eq!(
        links[0].span_context.span_id(),
        decision.span_context.span_id()
    );
    assert_eq!(links[0].span_context.trace_id(), release_trace);
    assert_eq!(
        attribute(&trade.attributes, "nexus.release_trace_id"),
        Some(release_trace.to_string())
    );
    assert_eq!(
        attribute(&trade.attributes, "analyst.resumed").as_deref(),
        Some("false")
    );

    let observations = spans_named(&harness, "analyst.observation");
    let horizons: Vec<String> = observations
        .iter()
        .filter_map(|o| attribute(&o.attributes, "analyst.horizon"))
        .collect();
    for horizon in ["1m", "5m", "15m", "30m", "60m", "close"] {
        assert!(horizons.contains(&horizon.to_owned()), "{horizon}");
    }
    assert!(observations
        .iter()
        .all(|o| o.parent_span_id == trade.span_context.span_id()
            && o.span_context.trace_id() == trade.span_context.trace_id()));
    assert!(observations
        .iter()
        .all(
            |o| attribute(&o.attributes, "analyst.observation_status").as_deref() == Some("ok")
                && attribute(&o.attributes, "analyst.late").as_deref() == Some("false")
        ));
    let exit = harness.span("analyst.exit");
    assert_eq!(exit.parent_span_id, trade.span_context.span_id());
    assert_eq!(exit.start_time, system_time(sto(11, 10, 11, 30, 0)));
    let purposes: Vec<String> = quotes
        .iter()
        .filter_map(|q| attribute(&q.attributes, "analyst.quote_purpose"))
        .collect();
    assert!(purposes.contains(&"exit".to_owned()));
    assert_eq!(purposes.iter().filter(|p| *p == "observation").count(), 6);

    let stored: StoredTrade = rig.store.trade();
    assert_eq!(
        stored.trace_id.as_deref(),
        Some(release_trace.to_string().as_str())
    );
    assert_eq!(
        stored.release_traceparent,
        Some(format!(
            "00-{release_trace}-{}-01",
            decision.span_context.span_id()
        ))
    );
    assert_eq!(
        stored.trade_trace_id.as_deref(),
        Some(trade.span_context.trace_id().to_string().as_str())
    );
    let last = rig
        .sink
        .messages()
        .into_iter()
        .map(|(_, m)| m)
        .max_by_key(|m| m.version)
        .expect("messages");
    assert_eq!(last.trace_id, release_trace.to_string());
    assert_eq!(
        last.trade_trace_id,
        trade.span_context.trace_id().to_string()
    );
    assert_eq!(last.status, "closed");
    assert_eq!(stored.status(), TradeStatus::Closed);
    assert_eq!(
        stored.pnl_pct.map(|p| p.to_string()).as_deref(),
        Some("1.5000")
    );
    assert_eq!(stored.exit_bid, Some(dec("101.50")));

    let sends = spans_named(&harness, "send analyst.trades");
    assert_eq!(sends.len(), rig.sink.messages().len());
    assert!(sends.iter().all(|s| s.span_kind == SpanKind::Producer));
    let entry_send = sends
        .iter()
        .find(|s| s.parent_span_id == entry.span_context.span_id())
        .expect("the entry message is a child of the entry span");
    assert_eq!(entry_send.span_context.trace_id(), release_trace);
    assert!(sends
        .iter()
        .filter(|s| s.parent_span_id != entry.span_context.span_id())
        .all(|s| s.span_context.trace_id() == trade.span_context.trace_id()));
    let headers = rig.sink.headers();
    assert_eq!(headers.len(), sends.len());
    for (_, sent) in &headers {
        let traceparent = sent
            .iter()
            .find(|(key, _)| key == "traceparent")
            .map(|(_, value)| value.clone())
            .expect("every message carries traceparent");
        assert!(sends.iter().any(|s| traceparent
            == format!(
                "00-{}-{}-01",
                s.span_context.trace_id(),
                s.span_context.span_id()
            )));
    }
}

#[tokio::test(start_paused = true)]
async fn a_pre_open_release_waits_for_the_session_in_its_own_span() {
    let harness = Harness::new();
    let _subscriber = tracing::subscriber::set_default(harness.subscriber());
    let released = sto(11, 10, 7, 30, 0);
    let rig = flat(released);
    let decision_span = tracing::info_span!("analyst.hand_off");
    let mut signal = signal(1, "AAA", "XSTO", DecisionKind::Buy, released);
    signal.context = decision_span.context();
    hand_off(&rig, signal);
    run(12 * 60).await;
    drop(decision_span);
    let wait = harness.span("analyst.wait_for_session");
    let entry = harness.span("analyst.trade_entry");
    assert_eq!(wait.parent_span_id, entry.span_context.span_id());
    assert_eq!(wait.start_time, system_time(released));
    assert_eq!(wait.end_time, system_time(sto(11, 10, 9, 0, 0)));
    assert_eq!(entry.end_time, system_time(sto(11, 10, 9, 0, 0)));
}

#[tokio::test(start_paused = true)]
async fn an_entry_that_never_comes_is_an_error_span_with_every_attempt() {
    let harness = Harness::new();
    let _subscriber = tracing::subscriber::set_default(harness.subscriber());
    let rig = rig(sto(11, 10, 11, 0, 0), Duration::ZERO, 8, |_| {
        let mut q = quote("101.50", "100.00");
        q.ask_type = Some("Indicative".into());
        Ok(q)
    });
    hand_off(
        &rig,
        signal(1, "AAA", "XSTO", DecisionKind::Buy, sto(11, 10, 11, 0, 0)),
    );
    run(10).await;
    let entry = harness.span("analyst.trade_entry");
    assert_eq!(
        attribute(&entry.attributes, "analyst.trade_status").as_deref(),
        Some("no_entry_quote")
    );
    assert!(matches!(entry.status, Status::Error { .. }));
    let attempts = spans_named(&harness, "analyst.quote");
    assert!(attempts.len() > 250);
    assert!(attempts
        .iter()
        .all(|q| attribute(&q.attributes, "analyst.accepted").as_deref() == Some("false")));
    assert!(
        spans_named(&harness, "analyst.trade").is_empty(),
        "no holding trace without an entry"
    );
}

fn open_trade(entry_at: DateTime<Utc>, traceparent: &str) -> StoredTrade {
    StoredTrade {
        id: 1,
        decision_id: 1,
        ticker: "AAA".into(),
        exchange_mic: "XSTO".into(),
        decision: "buy".into(),
        shadow: false,
        published_at: entry_at - ChronoDuration::minutes(90),
        hold_minutes: dec("30"),
        entry_target_at: Some(entry_at),
        entry_at: Some(entry_at),
        entry_ask: Some(dec("100.00")),
        entry_bid: Some(dec("99.50")),
        entry_saxo_at: None,
        exit_target_at: Some(entry_at + ChronoDuration::minutes(30)),
        exit_at: None,
        exit_bid: None,
        exit_ask: None,
        pnl_per_share: None,
        pnl_pct: None,
        currency: Some("SEK".into()),
        release_to_entry_ms: Some(1),
        status: "open".into(),
        version: 2,
        trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
        release_traceparent: Some(traceparent.to_owned()),
        trade_trace_id: Some("0af7651916cd43dd8448eb211c80319c".into()),
    }
}

#[tokio::test(start_paused = true)]
async fn after_a_restart_the_trade_gets_a_new_root_linked_to_the_same_release_span() {
    let harness = Harness::new();
    let _subscriber = tracing::subscriber::set_default(harness.subscriber());
    let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let rig = flat(sto(11, 10, 9, 5, 3));
    rig.store
        .seed(open_trade(sto(11, 10, 9, 0, 0), traceparent));
    assert_eq!(rig.runner.resume().await.expect("resume"), 1);
    run(12 * 60).await;

    let trade = harness.span("analyst.trade");
    assert_eq!(
        attribute(&trade.attributes, "analyst.resumed").as_deref(),
        Some("true")
    );
    assert_eq!(
        attribute(&trade.attributes, "nexus.release_trace_id").as_deref(),
        Some("4bf92f3577b34da6a3ce929d0e0e4736")
    );
    assert_eq!(trade.links.iter().count(), 1);
    let link = trade.links.iter().next().expect("link");
    assert_eq!(link.span_context.span_id().to_string(), "00f067aa0ba902b7");
    assert_ne!(
        trade.span_context.trace_id().to_string(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    let stored = rig.store.trade();
    assert_eq!(
        stored.trade_trace_id.as_deref(),
        Some(trade.span_context.trace_id().to_string().as_str()),
        "the new trace replaces the old one"
    );
    assert_eq!(stored.release_traceparent.as_deref(), Some(traceparent));

    let missed: Vec<SpanData> = spans_named(&harness, "analyst.observation")
        .into_iter()
        .filter(|o| {
            attribute(&o.attributes, "analyst.observation_status").as_deref() == Some("missed")
        })
        .collect();
    let horizons: Vec<String> = missed
        .iter()
        .filter_map(|o| attribute(&o.attributes, "analyst.horizon"))
        .collect();
    assert!(horizons.contains(&"5m".to_owned()));
    let five = missed
        .iter()
        .find(|o| attribute(&o.attributes, "analyst.horizon").as_deref() == Some("5m"))
        .expect("5m");
    assert_eq!(
        five.start_time, five.end_time,
        "a missed observation has zero length"
    );
    assert!(matches!(five.status, Status::Error { .. }));
    let on_time = spans_named(&harness, "analyst.observation")
        .into_iter()
        .find(|o| attribute(&o.attributes, "analyst.horizon").as_deref() == Some("15m"))
        .expect("15m");
    assert_eq!(
        attribute(&on_time.attributes, "analyst.observation_status").as_deref(),
        Some("ok")
    );
    let _: &dyn TradeStore = rig.store.as_ref();
}

#[tokio::test(start_paused = true)]
async fn a_pending_entry_resumed_after_a_restart_continues_the_release_trace() {
    let harness = Harness::new();
    let _subscriber = tracing::subscriber::set_default(harness.subscriber());
    let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let rig = flat(sto(11, 10, 8, 59, 0));
    let mut pending = open_trade(sto(11, 10, 9, 0, 0), traceparent);
    pending.status = "pending_entry".into();
    pending.entry_at = None;
    pending.entry_ask = None;
    pending.exit_target_at = None;
    rig.store.seed(pending);
    rig.runner.resume().await.expect("resume");
    run(3 * 60).await;
    let entry = harness.span("analyst.trade_entry");
    assert_eq!(
        entry.span_context.trace_id().to_string(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert_eq!(entry.parent_span_id.to_string(), "00f067aa0ba902b7");
    assert_eq!(
        attribute(&entry.attributes, "analyst.resumed").as_deref(),
        Some("true")
    );
    assert_eq!(spans_named(&harness, "analyst.wait_for_session").len(), 1);
}
