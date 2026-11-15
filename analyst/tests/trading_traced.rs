//! Simulated trades with tracing on and the collector unreachable: the entry
//! request still leaves within milliseconds and 50 trades at the open still
//! request within a second. Its own test binary because the tracing subscriber
//! is installed globally.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

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
