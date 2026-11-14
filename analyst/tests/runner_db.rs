//! The trade runner against the real store: a trade whose calendar is unknown,
//! and the rescheduling of open trades after a restart in which every target has
//! passed (nothing sleeps, nothing is fetched).

mod common;

use std::sync::Arc;
use std::time::Duration;

use analyst::{
    DecisionHandle, DecisionKind, Persisted, QuoteError, SessionsCache, Store, SystemClock,
    TradeHandOff, TradeRunner, TradeStatus,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use common::trading::{sessions, settings, signal, sto, FakeQuotes, MemSink};
use sqlx::PgPool;

async fn decision(pool: &PgPool, news_id: &str, kind: &str) -> i64 {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO decisions (news_id, ticker, exchange_mic, lang, published_at, received_at, decided_at, decision) \
         VALUES ($1, 'AAA', 'XSTO', 'en', now(), now(), now(), $2) RETURNING id",
    )
    .bind(news_id)
    .bind(kind)
    .fetch_one(pool)
    .await
    .expect("decision");
    row.0
}

fn runner(pool: &PgPool, sink: Arc<MemSink>) -> (TradeRunner, Arc<FakeQuotes>) {
    let clock = Arc::new(SystemClock);
    let quotes = FakeQuotes::new(clock.clone(), Duration::ZERO, |_| {
        Err::<analyst::Quote, _>(QuoteError::NotFound)
    });
    let runner = TradeRunner::new(
        Arc::new(Store::new(pool.clone())),
        SessionsCache::new(sessions()),
        quotes.clone(),
        sink,
        clock,
        settings(8),
    );
    (runner, quotes)
}

async fn wait_for<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the condition did not come true in 5 s");
}

#[sqlx::test]
async fn a_trade_for_an_unknown_exchange_is_stored_and_emitted_without_a_quote(pool: PgPool) {
    let sink = Arc::new(MemSink::default());
    let (runner, quotes) = runner(&pool, sink.clone());
    let id = decision(&pool, "n1", "buy").await;
    let mut signal = signal(id, "AAA", "XOSL", DecisionKind::Buy, Utc::now());
    signal.persisted = DecisionHandle::ready(Persisted {
        id,
        kind: DecisionKind::Buy,
    });
    runner.hand_off(signal);
    let store = Store::new(pool.clone());
    wait_for(|| async {
        store
            .trade_count(TradeStatus::CalendarUnknown)
            .await
            .expect("count")
            == 1
    })
    .await;
    assert!(quotes.times().is_empty());
    wait_for(|| async { !sink.messages().is_empty() }).await;
    let messages = sink.messages();
    assert_eq!(messages[0].0, "AAA:XOSL");
    assert_eq!(messages[0].1.status, "calendar_unknown");
    assert_eq!(messages[0].1.decision_id, id);
}

#[sqlx::test]
async fn open_trades_are_rescheduled_and_everything_that_passed_is_missed(pool: PgPool) {
    let sink = Arc::new(MemSink::default());
    let (runner, quotes) = runner(&pool, sink.clone());
    let id = decision(&pool, "n1", "buy").await;
    let entry_at: DateTime<Utc> = sto(11, 10, 9, 0, 0);
    sqlx::query(
        "INSERT INTO sim_trades (decision_id, ticker, exchange_mic, decision, shadow, published_at, hold_minutes, \
             entry_target_at, entry_at, entry_ask, currency, exit_target_at, status) \
         VALUES ($1, 'AAA', 'XSTO', 'buy', false, $2, 30, $3, $3, 100.00, 'SEK', $4, 'open')",
    )
    .bind(id)
    .bind(entry_at - ChronoDuration::hours(2))
    .bind(entry_at)
    .bind(entry_at + ChronoDuration::minutes(30))
    .execute(&pool)
    .await
    .expect("trade");
    assert_eq!(runner.resume().await.expect("resume"), 1);
    let store = Store::new(pool.clone());
    wait_for(|| async {
        store
            .trade_count(TradeStatus::ExitMissed)
            .await
            .expect("count")
            == 1
    })
    .await;
    let trade = store.open_trades().await.expect("open");
    assert!(trade.is_empty(), "an exit-missed trade has no quotes left");
    let all: (i64,) = sqlx::query_as("SELECT id FROM sim_trades")
        .fetch_one(&pool)
        .await
        .expect("trade");
    let observations = store.observations(all.0).await.expect("observations");
    assert_eq!(observations.len(), 7, "six horizons and the hold");
    assert!(observations.iter().all(|o| o.status == "missed"));
    assert!(
        quotes.times().is_empty(),
        "a late price would be a different price"
    );
}
