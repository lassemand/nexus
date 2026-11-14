//! Simulated trades with tracing on and the collector unreachable: the entry
//! request still leaves within milliseconds and 50 trades at the open still
//! request within a second. Its own test binary because the tracing subscriber
//! is installed globally.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use analyst::{DecisionKind, StoredTrade, TradeStatus};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use common::trading::{hand_off, quote, rig, signal, sto};

static GUARD: std::sync::OnceLock<telemetry::trace::TracingGuard> = std::sync::OnceLock::new();

fn init() {
    GUARD.get_or_init(|| {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:1");
        telemetry::trace::init("analyst-trades-test").expect("tracing starts")
    });
}

#[tokio::test(start_paused = true)]
async fn the_entry_request_leaves_within_five_milliseconds_with_tracing_on() {
    init();
    let released = sto(11, 10, 11, 0, 0);
    let mut elapsed = Vec::new();
    for id in 0..20 {
        let rig = rig(released, Duration::ZERO, 8, |_| {
            Ok(quote("101.50", "100.00"))
        });
        let decision_span = tracing::info_span!("analyst.hand_off");
        let mut signal = signal(id + 1, "AAA", "XSTO", DecisionKind::Buy, released);
        signal.context = telemetry::trace::OpenTelemetrySpanExt::context(&decision_span);
        let handed = Instant::now();
        hand_off(&rig, signal);
        loop {
            tokio::task::yield_now().await;
            if let Some(at) = rig.quotes.real_times.lock().expect("lock").first() {
                elapsed.push(at.duration_since(handed));
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(8 * 3600)).await;
    }
    elapsed.sort();
    let p99 = elapsed[elapsed.len() * 99 / 100];
    eprintln!(
        "hand-off to first quote request, tracing on, collector down: p50 {:?} p99 {p99:?} max {:?}",
        elapsed[elapsed.len() / 2],
        elapsed[elapsed.len() - 1]
    );
    assert!(p99 < Duration::from_millis(5), "p99 was {p99:?}");
}

#[tokio::test(start_paused = true)]
async fn fifty_trades_at_the_open_still_request_within_a_second_with_tracing_on() {
    init();
    let released = sto(11, 10, 8, 59, 0);
    let rig = rig(released, Duration::from_millis(50), 8, |_| {
        Ok(quote("101.50", "100.00"))
    });
    for id in 0..50 {
        hand_off(
            &rig,
            signal(
                id + 1,
                &format!("T{id:02}"),
                "XSTO",
                DecisionKind::Buy,
                released,
            ),
        );
    }
    tokio::time::sleep(Duration::from_secs(61)).await;
    let open = sto(11, 10, 9, 0, 0);
    let first: Vec<DateTime<Utc>> = {
        let mut seen = std::collections::HashMap::new();
        for (ticker, at) in rig.quotes.requests.lock().expect("lock").iter() {
            seen.entry(ticker.clone()).or_insert(*at);
        }
        seen.into_values().collect()
    };
    assert_eq!(first.len(), 50);
    assert!(first
        .iter()
        .all(|at| *at >= open && *at <= open + ChronoDuration::seconds(1)));
    assert!(rig.quotes.max_in_flight.load(Ordering::SeqCst) <= 8);
    tokio::time::sleep(Duration::from_secs(12 * 3600)).await;
    let trades: Vec<StoredTrade> = rig.store.trades();
    assert_eq!(trades.len(), 50);
    assert!(trades.iter().all(|t| t.status() == TradeStatus::Closed));
}
