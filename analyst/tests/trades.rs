//! Simulated trades in the analyst's database: creation, state changes with
//! versions, observations, and the `AnalystTrade` message, against a real Postgres.

use std::str::FromStr;

use analyst::{
    compute_pnl, trade_to_proto, Entry, Exit, NewObservation, NewTrade, ObservationStatus, Quote,
    Store, TradeStatus,
};
use chrono::{DateTime, Duration, TimeZone, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;

fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 10, h, m, s)
        .single()
        .expect("valid instant")
}

fn dec(text: &str) -> Decimal {
    Decimal::from_str(text).expect("decimal")
}

async fn decision(pool: &PgPool, news_id: &str, kind: &str) -> i64 {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO decisions (news_id, ticker, exchange_mic, lang, published_at, received_at, decided_at, decision) \
         VALUES ($1, 'AAA', 'XSTO', 'en', $2, $2, $2, $3) RETURNING id",
    )
    .bind(news_id)
    .bind(at(7, 0, 0))
    .bind(kind)
    .fetch_one(pool)
    .await
    .expect("decision");
    row.0
}

fn new_trade(decision_id: i64, kind: &str) -> NewTrade {
    NewTrade {
        decision_id,
        ticker: "AAA".into(),
        exchange_mic: "XSTO".into(),
        decision: kind.into(),
        published_at: at(7, 0, 0),
        hold_minutes: dec("30"),
        entry_target_at: Some(at(8, 0, 0)),
        status: TradeStatus::PendingEntry,
        trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
        release_traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into()),
    }
}

fn entry() -> Entry {
    Entry {
        at: at(8, 0, 0) + Duration::milliseconds(120),
        ask: dec("100.00"),
        bid: Some(dec("99.50")),
        saxo_at: Some("2026-11-10T08:00:00.100Z".into()),
        currency: Some("SEK".into()),
        release_to_entry_ms: 3_600_120,
        exit_target_at: Some(at(8, 30, 0)),
    }
}

fn quote(bid: &str) -> Quote {
    Quote {
        bid: Some(dec(bid)),
        ask: Some(dec("101.00")),
        bid_type: Some("Tradable".into()),
        ask_type: Some("Tradable".into()),
        delayed_minutes: Some(0),
        currency: Some("SEK".into()),
        saxo_last_updated: None,
    }
}

#[sqlx::test]
async fn a_decision_has_one_trade_and_only_a_buy_is_not_a_shadow(pool: PgPool) {
    let store = Store::new(pool.clone());
    let buy = decision(&pool, "n1", "buy").await;
    let trade = store
        .create_trade(&new_trade(buy, "buy"))
        .await
        .expect("create")
        .expect("created");
    assert!(!trade.shadow);
    assert_eq!(trade.status(), TradeStatus::PendingEntry);
    assert_eq!(trade.version, 1);
    assert!(store
        .create_trade(&new_trade(buy, "buy"))
        .await
        .expect("create")
        .is_none());
    for (news, kind) in [
        ("n2", "no_buy"),
        ("n3", "abstain"),
        ("n4", "skipped_no_kernel"),
    ] {
        let id = decision(&pool, news, kind).await;
        let shadow = store
            .create_trade(&new_trade(id, kind))
            .await
            .expect("create")
            .expect("created");
        assert!(shadow.shadow, "{kind} is a shadow trade");
    }
    let stale = decision(&pool, "n5", "stale").await;
    assert!(store
        .create_trade(&new_trade(stale, "stale"))
        .await
        .is_err());
}

#[sqlx::test]
async fn every_state_change_bumps_the_version_and_a_closed_trade_has_its_result(pool: PgPool) {
    let store = Store::new(pool.clone());
    let id = decision(&pool, "n1", "buy").await;
    let trade = store
        .create_trade(&new_trade(id, "buy"))
        .await
        .expect("create")
        .expect("created");
    let opened = store.record_entry(trade.id, &entry()).await.expect("entry");
    assert_eq!((opened.status(), opened.version), (TradeStatus::Open, 2));
    assert_eq!(opened.entry_ask, Some(dec("100.00")));
    assert_eq!(opened.release_to_entry_ms, Some(3_600_120));
    assert_eq!(opened.currency.as_deref(), Some("SEK"));

    let observed = store
        .record_observation(
            trade.id,
            &NewObservation {
                horizon: "1m".into(),
                scheduled_at: at(8, 1, 0),
                fetched_at: Some(at(8, 1, 0) + Duration::milliseconds(40)),
                quote: Some(quote("100.40")),
                late: false,
                status: ObservationStatus::Ok,
            },
        )
        .await
        .expect("observation");
    assert_eq!(observed.version, 3);
    store
        .record_observation(
            trade.id,
            &NewObservation {
                horizon: "60m".into(),
                scheduled_at: at(9, 0, 0),
                fetched_at: None,
                quote: None,
                late: false,
                status: ObservationStatus::BeyondClose,
            },
        )
        .await
        .expect("observation");

    let pnl = compute_pnl(dec("100.00"), dec("101.50"), Some("SEK"), Some("SEK")).expect("pnl");
    let closed = store
        .record_exit(
            trade.id,
            &Exit {
                at: Some(at(8, 30, 0)),
                bid: Some(dec("101.50")),
                ask: Some(dec("101.80")),
                pnl: Some(pnl),
                status: TradeStatus::Closed,
            },
        )
        .await
        .expect("exit");
    assert_eq!((closed.status(), closed.version), (TradeStatus::Closed, 5));
    assert_eq!(
        closed.pnl_pct.map(|p| p.to_string()).as_deref(),
        Some("1.5000")
    );
    assert_eq!(closed.pnl_per_share, Some(dec("1.50")));

    let observations = store.observations(trade.id).await.expect("observations");
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].horizon, "1m");
    assert_eq!(observations[0].bid, Some(dec("100.40")));
    assert_eq!(observations[1].status, "beyond_close");

    let message = trade_to_proto(&closed, &observations);
    assert_eq!(message.version, 5);
    assert_eq!(message.entry_ask, "100.000000");
    assert_eq!(message.exit_bid, "101.500000");
    assert_eq!(message.pnl_pct, "1.5000");
    assert_eq!(message.hold_minutes, "30");
    assert_eq!(message.entry_at_unix_millis, entry().at.timestamp_millis());
    assert_eq!(message.release_to_entry_ms, 3_600_120);
    assert_eq!(message.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(message.trade_trace_id, "");
    assert_eq!(message.observations.len(), 2);
    assert_eq!(message.observations[1].fetched_at_unix_millis, 0);
    assert_eq!(message.observations[1].delayed_minutes, -1);
    assert!(!message.shadow);
}

#[sqlx::test]
async fn only_trades_with_quotes_left_are_open_and_an_observation_is_replaced(pool: PgPool) {
    let store = Store::new(pool.clone());
    let first = decision(&pool, "n1", "buy").await;
    let second = decision(&pool, "n2", "no_buy").await;
    let a = store
        .create_trade(&new_trade(first, "buy"))
        .await
        .expect("create")
        .expect("created");
    let b = store
        .create_trade(&new_trade(second, "no_buy"))
        .await
        .expect("create")
        .expect("created");
    store.record_entry(b.id, &entry()).await.expect("entry");
    assert_eq!(store.open_trades().await.expect("open").len(), 2);
    store
        .set_trade_status(a.id, TradeStatus::NoEntryQuote)
        .await
        .expect("status");
    let open = store.open_trades().await.expect("open");
    assert_eq!(open.iter().map(|t| t.id).collect::<Vec<_>>(), [b.id]);
    assert_eq!(
        store
            .trade_count(TradeStatus::NoEntryQuote)
            .await
            .expect("count"),
        1
    );

    let missed = NewObservation {
        horizon: "5m".into(),
        scheduled_at: at(8, 5, 0),
        fetched_at: None,
        quote: None,
        late: false,
        status: ObservationStatus::Missed,
    };
    store
        .record_observation(b.id, &missed)
        .await
        .expect("observation");
    store
        .record_observation(b.id, &missed)
        .await
        .expect("again");
    assert_eq!(
        store.observations(b.id).await.expect("observations").len(),
        1
    );
    store
        .set_trade_trace(b.id, "0af7651916cd43dd8448eb211c80319c")
        .await
        .expect("trace");
    let stored = store.trade(b.id).await.expect("trade").expect("exists");
    assert_eq!(
        stored.trade_trace_id.as_deref(),
        Some("0af7651916cd43dd8448eb211c80319c")
    );
}

#[sqlx::test]
async fn the_database_rejects_a_bad_status_trace_id_or_hold(pool: PgPool) {
    let store = Store::new(pool.clone());
    let id = decision(&pool, "n1", "buy").await;
    let trade = store
        .create_trade(&new_trade(id, "buy"))
        .await
        .expect("create")
        .expect("created");
    assert!(
        sqlx::query("UPDATE sim_trades SET status = 'bought' WHERE id = $1")
            .bind(trade.id)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(store.set_trade_trace(trade.id, "NOT-HEX").await.is_err());
    assert!(
        sqlx::query("UPDATE sim_trades SET hold_minutes = 0 WHERE id = $1")
            .bind(trade.id)
            .execute(&pool)
            .await
            .is_err()
    );
}
