//! Simulated trades end to end with tokio's time paused, a clock that follows it,
//! an in-memory store and a scripted quote source: the instants of entry, exit
//! and every observation, the quote rules, restarts and concurrency.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use analyst::{DecisionKind, QuoteError, StoredTrade, TradeStatus};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use common::trading::{dec, hand_off, horizons_of, quote, rig, signal, sto, versions, Rig};

const HOURS: u64 = 3600;

async fn run(rig: &Rig, minutes: u64) {
    tokio::time::sleep(Duration::from_secs(minutes * 60)).await;
    let _ = rig;
}

fn flat(
    bid: &'static str,
    ask: &'static str,
) -> impl Fn(DateTime<Utc>) -> Result<analyst::Quote, QuoteError> + Send + Sync {
    move |_| Ok(quote(bid, ask))
}

fn assert_entry_exit(trade: &StoredTrade, entry: DateTime<Utc>, exit: DateTime<Utc>) {
    assert_eq!(trade.entry_at, Some(entry), "entry");
    assert_eq!(trade.exit_at, Some(exit), "exit");
    assert_eq!(trade.status(), TradeStatus::Closed);
}

#[tokio::test(start_paused = true)]
async fn a_release_before_the_open_enters_at_the_open_and_exits_thirty_minutes_later() {
    let released = sto(11, 10, 7, 30, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    run(&rig, 12 * 60).await;
    let trade = rig.store.trade();
    assert_entry_exit(&trade, sto(11, 10, 9, 0, 0), sto(11, 10, 9, 30, 0));
    assert_eq!(trade.entry_target_at, Some(sto(11, 10, 9, 0, 0)));
    assert_eq!(trade.release_to_entry_ms, Some(90 * 60 * 1000 + 300));
    assert!(!trade.shadow);
    assert_eq!(
        trade.pnl_pct.map(|p| p.to_string()).as_deref(),
        Some("1.5000")
    );
    assert_eq!(trade.pnl_per_share, Some(dec("1.50")));
}

#[tokio::test(start_paused = true)]
async fn a_release_during_trading_enters_at_once_and_exits_after_the_hold() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        rig.quotes.times(),
        [released],
        "the entry request leaves within 5 ms of the decision"
    );
    run(&rig, 6 * 60).await;
    assert_entry_exit(&rig.store.trade(), released, sto(11, 10, 11, 30, 0));
}

#[tokio::test(start_paused = true)]
async fn a_late_entry_exits_at_the_cutoff_not_after_the_hold() {
    let released = sto(11, 10, 17, 10, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    run(&rig, 60).await;
    assert_entry_exit(&rig.store.trade(), released, sto(11, 10, 17, 25, 0));
}

#[tokio::test(start_paused = true)]
async fn a_release_after_the_close_enters_the_next_trading_day() {
    let released = sto(11, 10, 18, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    run(&rig, 20 * 60).await;
    assert_entry_exit(
        &rig.store.trade(),
        sto(11, 11, 9, 0, 0),
        sto(11, 11, 9, 30, 0),
    );
}

#[tokio::test(start_paused = true)]
async fn a_friday_release_before_a_monday_holiday_enters_on_tuesday() {
    let released = sto(11, 13, 18, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    run(&rig, 5 * 24 * 60).await;
    assert_entry_exit(
        &rig.store.trade(),
        sto(11, 17, 9, 0, 0),
        sto(11, 17, 9, 30, 0),
    );
}

#[tokio::test(start_paused = true)]
async fn a_helsinki_company_enters_at_the_helsinki_open() {
    use chrono::TimeZone;
    let helsinki = |h, m| {
        chrono_tz::Europe::Helsinki
            .with_ymd_and_hms(2026, 11, 10, h, m, 0)
            .single()
            .expect("instant")
            .with_timezone(&Utc)
    };
    let released = helsinki(9, 30);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "BBB", "XHEL", DecisionKind::Buy, released));
    run(&rig, 12 * 60).await;
    assert_entry_exit(&rig.store.trade(), helsinki(10, 0), helsinki(10, 30));
}

#[tokio::test(start_paused = true)]
async fn on_a_half_day_the_cutoff_is_five_minutes_before_the_early_close() {
    let released = sto(12, 24, 12, 40, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    run(&rig, 60).await;
    assert_entry_exit(&rig.store.trade(), released, sto(12, 24, 12, 55, 0));
}

#[tokio::test(start_paused = true)]
async fn an_indicative_quote_for_three_seconds_then_a_tradable_one_enters_at_the_tradable_one() {
    let open = sto(11, 10, 9, 0, 0);
    let rig = rig(sto(11, 10, 8, 0, 0), Duration::ZERO, 8, move |now| {
        let mut q = quote("101.50", "100.00");
        if now < open + ChronoDuration::seconds(3) {
            q.ask_type = Some("Indicative".into());
        }
        Ok(q)
    });
    hand_off(
        &rig,
        signal(1, "AAA", "XSTO", DecisionKind::Buy, sto(11, 10, 8, 0, 0)),
    );
    run(&rig, 61).await;
    let trade = rig.store.trade();
    assert_eq!(trade.entry_at, Some(open + ChronoDuration::seconds(3)));
    assert_eq!(
        trade.status(),
        TradeStatus::Open,
        "the exit is hold minutes later"
    );
}

#[tokio::test(start_paused = true)]
async fn a_quote_that_is_never_tradable_gives_no_entry_quote_after_the_wait() {
    let rig = rig(sto(11, 10, 11, 0, 0), Duration::ZERO, 8, |_| {
        let mut q = quote("101.50", "100.00");
        q.ask_type = Some("Indicative".into());
        Ok(q)
    });
    hand_off(
        &rig,
        signal(1, "AAA", "XSTO", DecisionKind::Buy, sto(11, 10, 11, 0, 0)),
    );
    run(&rig, 10).await;
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::NoEntryQuote);
    assert_eq!(trade.entry_at, None);
    let times = rig.quotes.times();
    assert_eq!(times[0], sto(11, 10, 11, 0, 0));
    assert!(*times.last().expect("requests") <= sto(11, 10, 11, 5, 0));
    assert!(
        times.len() > 250,
        "one request a second for five minutes: {}",
        times.len()
    );
}

#[tokio::test(start_paused = true)]
async fn a_delayed_quote_and_a_zero_ask_are_not_accepted() {
    for variant in 0..2 {
        let rig = rig(sto(11, 10, 11, 0, 0), Duration::ZERO, 8, move |_| {
            let mut q = quote("101.50", "100.00");
            if variant == 0 {
                q.delayed_minutes = Some(5);
            } else {
                q.ask = Some(dec("0"));
            }
            Ok(q)
        });
        hand_off(
            &rig,
            signal(1, "AAA", "XSTO", DecisionKind::Buy, sto(11, 10, 11, 0, 0)),
        );
        run(&rig, 10).await;
        assert_eq!(
            rig.store.trade().status(),
            TradeStatus::NoEntryQuote,
            "variant {variant}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_503_is_waited_out_for_as_long_as_it_asks() {
    let start = sto(11, 10, 11, 0, 0);
    let rig = rig(start, Duration::ZERO, 8, move |now| {
        if now < start + ChronoDuration::seconds(10) {
            Err(QuoteError::Unavailable {
                retry_after: Duration::from_secs(4),
            })
        } else {
            Ok(quote("101.50", "100.00"))
        }
    });
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, start));
    run(&rig, 5).await;
    let times = rig.quotes.times();
    assert_eq!(
        &times[..4],
        [
            start,
            start + ChronoDuration::seconds(4),
            start + ChronoDuration::seconds(8),
            start + ChronoDuration::seconds(12)
        ]
    );
    assert_eq!(
        rig.store.trade().entry_at,
        Some(start + ChronoDuration::seconds(12))
    );
}

#[tokio::test(start_paused = true)]
async fn every_horizon_is_observed_and_close_is_fetched_at_the_cutoff() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    run(&rig, 8 * 60).await;
    let trade = rig.store.trade();
    let observations = rig.store.observations_of(trade.id);
    for label in ["1m", "5m", "15m", "30m", "60m", "close", "hold"] {
        assert!(horizons_of(&observations).contains(label), "{label}");
    }
    let close = observations
        .iter()
        .find(|o| o.horizon == "close")
        .expect("close");
    assert_eq!(close.fetched_at, Some(sto(11, 10, 17, 25, 0)));
    assert_eq!(close.status, "ok");
    assert!(!close.late);
    let hold = observations
        .iter()
        .find(|o| o.horizon == "hold")
        .expect("hold");
    assert_eq!(hold.scheduled_at, sto(11, 10, 11, 30, 0));
    assert_eq!(hold.bid, Some(dec("101.50")));
}

#[tokio::test(start_paused = true)]
async fn a_horizon_past_the_cutoff_is_beyond_close() {
    let released = sto(11, 10, 17, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    run(&rig, 60).await;
    let trade = rig.store.trade();
    let status = |label: &str| {
        rig.store
            .observations_of(trade.id)
            .into_iter()
            .find(|o| o.horizon == label)
            .map(|o| o.status)
    };
    assert_eq!(status("15m").as_deref(), Some("ok"));
    assert_eq!(status("30m").as_deref(), Some("beyond_close"));
    assert_eq!(status("60m").as_deref(), Some("beyond_close"));
    assert_eq!(status("close").as_deref(), Some("ok"));
}

#[tokio::test(start_paused = true)]
async fn a_closed_trade_in_another_currency_has_no_result() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, move |now| {
        let mut q = quote("101.50", "100.00");
        if now >= released + ChronoDuration::minutes(29) {
            q.currency = Some("EUR".into());
        }
        Ok(q)
    });
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    run(&rig, 60).await;
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::CurrencyMismatch);
    assert_eq!(trade.pnl_pct, None);
    assert_eq!(trade.exit_bid, Some(dec("101.50")));
}

#[tokio::test(start_paused = true)]
async fn an_exit_quote_that_never_comes_gives_no_exit_quote() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, move |now| {
        let mut q = quote("101.50", "100.00");
        if now >= released + ChronoDuration::minutes(30) {
            q.bid_type = Some("Indicative".into());
        }
        Ok(q)
    });
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    run(&rig, 60).await;
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::NoExitQuote);
    let hold = rig
        .store
        .observations_of(trade.id)
        .into_iter()
        .find(|o| o.horizon == "hold")
        .expect("hold");
    assert_eq!(hold.status, "no_quote");
}

#[tokio::test(start_paused = true)]
async fn a_no_buy_is_a_shadow_trade_and_a_decision_that_is_not_traded_gets_none() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(
        &rig,
        signal(1, "AAA", "XSTO", DecisionKind::NoBuy, released),
    );
    hand_off(
        &rig,
        signal(2, "BBB", "XSTO", DecisionKind::Abstain, released),
    );
    hand_off(
        &rig,
        signal(3, "CCC", "XSTO", DecisionKind::SkippedNoKernel, released),
    );
    for (id, kind) in [
        (4, DecisionKind::Stale),
        (5, DecisionKind::KernelError),
        (6, DecisionKind::TwinIgnored),
    ] {
        hand_off(&rig, signal(id, "DDD", "XSTO", kind, released));
    }
    run(&rig, 60).await;
    let trades = rig.store.trades();
    assert_eq!(trades.len(), 3);
    assert!(trades.iter().all(|t| t.shadow));
    assert!(trades.iter().all(|t| t.status() == TradeStatus::Closed));
    assert!(rig
        .quotes
        .requests
        .lock()
        .expect("lock")
        .iter()
        .all(|(t, _)| t != "DDD"));
}

#[tokio::test(start_paused = true)]
async fn a_decision_that_lost_a_race_or_was_never_stored_is_not_traded() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    let mut downgraded = signal(1, "AAA", "XSTO", DecisionKind::Buy, released);
    downgraded.persisted = analyst::DecisionHandle::ready(analyst::Persisted {
        id: 1,
        kind: DecisionKind::TwinIgnored,
    });
    hand_off(&rig, downgraded);
    let (slot, handle) = analyst::decision_channel();
    let mut dropped = signal(2, "BBB", "XSTO", DecisionKind::Buy, released);
    dropped.persisted = handle;
    hand_off(&rig, dropped);
    drop(slot);
    run(&rig, 60).await;
    assert!(rig.store.trades().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_unknown_exchange_or_year_is_calendar_unknown_and_no_quote_is_requested() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "AAA", "XOSL", DecisionKind::Buy, released));
    hand_off(
        &rig,
        signal(
            2,
            "BBB",
            "XSTO",
            DecisionKind::Buy,
            released + ChronoDuration::days(400),
        ),
    );
    run(&rig, 60).await;
    let trades = rig.store.trades();
    assert_eq!(trades.len(), 2);
    assert!(trades
        .iter()
        .all(|t| t.status() == TradeStatus::CalendarUnknown));
    assert!(rig.quotes.times().is_empty());
}

#[tokio::test(start_paused = true)]
async fn every_state_change_emits_the_full_trade_with_a_rising_version() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    run(&rig, 8 * 60).await;
    let trade = rig.store.trade();
    let seen = versions(&rig.sink, trade.id);
    let expected: Vec<i64> = (2..=trade.version).collect();
    assert_eq!(
        seen, expected,
        "entry, each observation, the hold quote and the exit"
    );
    let last = rig
        .sink
        .messages()
        .into_iter()
        .map(|(_, m)| m)
        .max_by_key(|m| m.version)
        .expect("messages");
    assert_eq!(last.observations.len(), 7);
    assert_eq!(last.status, "closed");
    assert_eq!(last.pnl_pct, "1.5000");
    assert!(rig.sink.messages().iter().all(|(k, _)| k == "AAA:XSTO"));
}

fn open_trade(entry_at: DateTime<Utc>) -> StoredTrade {
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
        trace_id: None,
        release_traceparent: None,
        trade_trace_id: None,
    }
}

async fn restart(restart_at: DateTime<Utc>) -> Rig {
    let rig = rig(restart_at, Duration::ZERO, 8, flat("101.50", "100.00"));
    rig.store.seed(open_trade(sto(11, 10, 9, 0, 0)));
    assert_eq!(rig.runner.resume().await.expect("resume"), 1);
    tokio::time::sleep(Duration::from_secs(12 * HOURS)).await;
    rig
}

#[tokio::test(start_paused = true)]
async fn after_a_restart_a_quote_three_seconds_late_is_missed_and_the_next_is_on_time() {
    let rig = restart(sto(11, 10, 9, 5, 3)).await;
    let trade = rig.store.trade();
    let observation = |label: &str| {
        rig.store
            .observations_of(trade.id)
            .into_iter()
            .find(|o| o.horizon == label)
            .unwrap_or_else(|| panic!("no {label}"))
    };
    assert_eq!(observation("1m").status, "missed");
    assert_eq!(observation("5m").status, "missed");
    assert_eq!(observation("5m").fetched_at, None);
    let fifteen = observation("15m");
    assert_eq!(fifteen.status, "ok");
    assert_eq!(fifteen.fetched_at, Some(sto(11, 10, 9, 15, 0)));
    assert!(!fifteen.late);
    assert_eq!(trade.status(), TradeStatus::Closed);
    assert_eq!(trade.exit_at, Some(sto(11, 10, 9, 30, 0)));
}

#[tokio::test(start_paused = true)]
async fn a_quote_one_second_late_after_a_restart_is_still_fetched() {
    let rig = restart(sto(11, 10, 9, 5, 1)).await;
    let five = rig
        .store
        .observations_of(1)
        .into_iter()
        .find(|o| o.horizon == "5m")
        .expect("5m");
    assert_eq!(five.status, "ok");
    assert!(!five.late, "one second is inside the two the task allows");
}

#[tokio::test(start_paused = true)]
async fn after_a_restart_past_the_exit_the_trade_is_exit_missed() {
    let rig = restart(sto(11, 10, 9, 30, 5)).await;
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::ExitMissed);
    let hold = rig
        .store
        .observations_of(trade.id)
        .into_iter()
        .find(|o| o.horizon == "hold")
        .expect("hold");
    assert_eq!(hold.status, "missed");
}

#[tokio::test(start_paused = true)]
async fn a_pending_entry_is_awaited_after_a_restart_unless_its_target_passed() {
    for (restart_at, expected) in [
        (sto(11, 10, 8, 59, 0), TradeStatus::Open),
        (sto(11, 10, 9, 0, 5), TradeStatus::NoEntryQuote),
    ] {
        let rig = rig(restart_at, Duration::ZERO, 8, flat("101.50", "100.00"));
        let mut pending = open_trade(sto(11, 10, 9, 0, 0));
        pending.status = "pending_entry".into();
        pending.entry_at = None;
        pending.entry_ask = None;
        pending.exit_target_at = None;
        pending.release_to_entry_ms = None;
        rig.store.seed(pending);
        rig.runner.resume().await.expect("resume");
        tokio::time::sleep(Duration::from_secs(3 * 60)).await;
        let status = rig.store.trade().status();
        assert_eq!(status, expected, "restart at {restart_at}");
    }
}

#[tokio::test(start_paused = true)]
async fn fifty_trades_at_the_open_all_request_within_a_second_and_never_exceed_the_limit() {
    let released = sto(11, 10, 8, 59, 0);
    let rig = rig(
        released,
        Duration::from_millis(50),
        8,
        flat("101.50", "100.00"),
    );
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
    assert!(
        rig.quotes.max_in_flight.load(Ordering::SeqCst) >= 2,
        "requests do run concurrently"
    );
    tokio::time::sleep(Duration::from_secs(12 * HOURS)).await;
    assert_eq!(rig.store.trades().len(), 50);
    assert!(rig
        .store
        .trades()
        .iter()
        .all(|t| t.status() == TradeStatus::Closed));
}
