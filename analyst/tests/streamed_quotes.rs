//! Simulated trades priced from the quote book (NEX-271), with tokio's time
//! paused: a book fed the way the stream client feeds it, a scripted request
//! source for the fallback, and an in-memory store.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use analyst::quote_book::key;
use analyst::{DecisionKind, Quote, QuoteBook, TradeStatus};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use common::trading::{dec, hand_off, quote, rig, signal, sto, Rig};
use tokio::task::JoinHandle;

fn flat(
    bid: &'static str,
    ask: &'static str,
) -> impl Fn(DateTime<Utc>) -> Result<Quote, analyst::QuoteError> + Send + Sync {
    move |_| Ok(quote(bid, ask))
}

fn up(book: &Arc<QuoteBook>) {
    book.on_connected();
    book.on_heartbeat(true);
}

fn keep_alive(book: &Arc<QuoteBook>) -> JoinHandle<()> {
    let book = Arc::clone(book);
    tokio::spawn(async move {
        loop {
            book.on_heartbeat(true);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

fn held(
    book: &QuoteBook,
    ticker: &str,
    bid: &str,
    ask: &str,
    kind: &str,
    saxo: DateTime<Utc>,
    seq: u64,
) {
    let mut q = quote(bid, ask);
    q.ask_type = Some(kind.into());
    q.bid_type = Some(kind.into());
    q.saxo_last_updated = Some(saxo.to_rfc3339());
    book.apply(&key(ticker, "XSTO"), q, seq);
}

fn tradable(book: &QuoteBook, ticker: &str, bid: &str, ask: &str, saxo: DateTime<Utc>, seq: u64) {
    held(book, ticker, bid, ask, "Tradable", saxo, seq);
}

fn scheduled(rig: &Rig, tickers: &[&str]) {
    rig.book
        .set_scheduled(tickers.iter().map(|t| key(t, "XSTO")).collect());
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(5)).await;
}

fn ms(n: i64) -> ChronoDuration {
    ChronoDuration::milliseconds(n)
}

#[tokio::test(start_paused = true)]
async fn an_open_market_and_a_healthy_stream_enter_at_the_decision_from_memory() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["AAA"]);
    tradable(
        &rig.book,
        "AAA",
        "99.50",
        "100.00",
        released - ms(30_000),
        1,
    );
    let alive = keep_alive(&rig.book);
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    settle().await;
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::Open);
    assert_eq!(trade.entry_at, Some(released), "the decision instant");
    assert_eq!(trade.entry_ask, Some(dec("100.00")));
    assert_eq!(trade.entry_quote_source.as_deref(), Some("stream"));
    assert_eq!(trade.entry_quote_saxo_time, Some(released - ms(30_000)));
    assert_eq!(trade.entry_quote_age_ms, Some(30_000));
    assert_eq!(trade.release_to_entry_ms, Some(300));
    assert!(rig.quotes.times().is_empty(), "no request was made");

    tokio::time::sleep(Duration::from_secs(29 * 60)).await;
    tradable(
        &rig.book,
        "AAA",
        "101.50",
        "101.60",
        released + ChronoDuration::minutes(29),
        2,
    );
    tokio::time::sleep(Duration::from_secs(2 * 60)).await;
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::Closed);
    assert_eq!(trade.exit_at, Some(sto(11, 10, 11, 30, 0)));
    assert_eq!(trade.exit_bid, Some(dec("101.50")));
    assert_eq!(
        trade.pnl_pct.map(|p| p.to_string()).as_deref(),
        Some("1.5000")
    );
    assert_eq!(trade.exit_quote_source.as_deref(), Some("stream"));
    assert!(
        rig.book.is_watched(&key("AAA", "XSTO")),
        "the 60 minute and close observations still need the stream"
    );

    tokio::time::sleep(Duration::from_secs(7 * 3600)).await;
    alive.abort();
    let observations = rig.store.observations_of(trade.id);
    assert_eq!(observations.len(), 7, "1m 5m 15m 30m 60m close hold");
    assert!(observations
        .iter()
        .all(|o| o.status == "ok" && o.quote_source.as_deref() == Some("stream")));
    assert!(
        rig.quotes.times().is_empty(),
        "a healthy stream never sends a request, entry to the last observation"
    );
    rig.book.set_scheduled(Default::default());
    assert!(
        !rig.book.is_watched(&key("AAA", "XSTO")),
        "unpinned when the trade's task ends"
    );
}

#[tokio::test(start_paused = true)]
async fn the_entry_price_is_read_before_hand_off_returns() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["AAA"]);
    tradable(&rig.book, "AAA", "99.50", "100.00", released, 1);
    let mut elapsed = Vec::new();
    for id in 1..=200 {
        let signal = signal(id, "AAA", "XSTO", DecisionKind::Buy, released);
        let started = Instant::now();
        hand_off(&rig, signal);
        elapsed.push(started.elapsed());
    }
    elapsed.sort();
    let p99 = elapsed[elapsed.len() * 99 / 100];
    eprintln!("hand_off with the price read, 200 samples: p99 {p99:?}");
    if !cfg!(debug_assertions) {
        assert!(p99 < Duration::from_millis(1), "p99 {p99:?}");
    }
    settle().await;
    assert!(rig.quotes.times().is_empty());
    assert!(
        rig.store
            .trades()
            .iter()
            .all(|t| t.entry_at == Some(released)
                && t.entry_quote_source.as_deref() == Some("stream"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_pre_open_release_enters_on_the_first_tradable_update_after_the_open() {
    let released = sto(11, 10, 7, 30, 0);
    let open = sto(11, 10, 9, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["AAA"]);
    held(
        &rig.book,
        "AAA",
        "99.00",
        "101.00",
        "Indicative",
        sto(11, 10, 8, 59, 0),
        1,
    );
    let alive = keep_alive(&rig.book);
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    tokio::time::sleep(Duration::from_secs(90 * 60)).await;
    assert_eq!(rig.store.trade().status(), TradeStatus::PendingEntry);
    tokio::time::sleep(Duration::from_millis(100)).await;
    held(
        &rig.book,
        "AAA",
        "99.00",
        "101.00",
        "Indicative",
        open + ms(100),
        2,
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    tradable(&rig.book, "AAA", "99.50", "100.00", open - ms(1000), 3);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rig.store.trade().status(),
        TradeStatus::PendingEntry,
        "a tradable quote stamped before the open is not the opening price"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    tradable(&rig.book, "AAA", "99.50", "100.40", open + ms(400), 4);
    settle().await;
    alive.abort();
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::Open);
    assert_eq!(trade.entry_at, Some(open + ms(400)));
    assert_eq!(trade.entry_ask, Some(dec("100.40")));
    assert_eq!(trade.entry_quote_source.as_deref(), Some("stream"));
    assert_eq!(trade.entry_quote_saxo_time, Some(open + ms(400)));
    assert!(rig.quotes.times().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_non_tradable_held_quote_waits_for_the_next_update_then_gives_up() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["AAA", "BBB"]);
    held(
        &rig.book,
        "AAA",
        "99.00",
        "101.00",
        "Indicative",
        released,
        1,
    );
    let alive = keep_alive(&rig.book);
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    hand_off(&rig, signal(2, "BBB", "XSTO", DecisionKind::Buy, released));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(rig
        .store
        .trades()
        .iter()
        .all(|t| t.status() == TradeStatus::PendingEntry));
    tradable(&rig.book, "AAA", "99.50", "100.00", released + ms(2_000), 2);
    settle().await;
    let a = rig.store.trades().into_iter().find(|t| t.ticker == "AAA");
    let a = a.expect("trade");
    assert_eq!(a.status(), TradeStatus::Open);
    assert_eq!(a.entry_at, Some(released + ms(2_000)));
    assert_eq!(a.entry_quote_source.as_deref(), Some("stream"));
    tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    alive.abort();
    let b = rig
        .store
        .trades()
        .into_iter()
        .find(|t| t.ticker == "BBB")
        .expect("trade");
    assert_eq!(b.status(), TradeStatus::NoEntryQuote);
    assert!(rig.quotes.times().is_empty());
}

#[tokio::test(start_paused = true)]
async fn when_the_stream_is_down_at_the_decision_a_fresh_enough_quote_is_requested() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    scheduled(&rig, &["AAA"]);
    rig.quotes.set_age(Duration::from_secs(10 * 60));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    settle().await;
    assert_eq!(rig.quotes.times(), [released]);
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::Open);
    assert_eq!(trade.entry_quote_source.as_deref(), Some("request"));
    assert_eq!(trade.entry_quote_age_ms, Some(10 * 60 * 1000));
}

#[tokio::test(start_paused = true)]
async fn a_requested_entry_quote_older_than_the_limit_cancels_the_trade_not_the_decision() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    scheduled(&rig, &["AAA"]);
    rig.quotes.set_age(Duration::from_secs(2 * 3600));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    tokio::time::sleep(Duration::from_secs(3 * 60 * 60)).await;
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::QuoteTooOld);
    assert_eq!(trade.entry_ask, None);
    assert_eq!(trade.entry_at, None);
    assert_eq!(trade.exit_at, None);
    assert_eq!(trade.pnl_pct, None);
    assert_eq!(trade.entry_quote_source.as_deref(), Some("request"));
    assert_eq!(trade.entry_quote_age_ms, Some(2 * 3600 * 1000));
    assert!(rig.store.observations_of(trade.id).is_empty());
    assert_eq!(rig.quotes.times().len(), 1, "one request, not retried");
    assert_eq!(trade.decision, "buy", "the decision stays what it was");
    let last = rig.sink.messages().last().expect("emitted").1.clone();
    assert_eq!(last.status, "quote_too_old");
    assert_eq!(last.entry_quote_source, "request");
    assert_eq!(last.entry_quote_age_ms, 2 * 3600 * 1000);
}

#[tokio::test(start_paused = true)]
async fn a_quote_without_a_readable_saxo_time_is_never_fresh() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, |_| {
        let mut q = quote("101.50", "100.00");
        q.saxo_last_updated = Some("not a time".into());
        Ok(q)
    });
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    settle().await;
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::QuoteTooOld);
    assert_eq!(trade.entry_quote_saxo_time, None);
    assert_eq!(trade.entry_quote_age_ms, None);
}

#[tokio::test(start_paused = true)]
async fn a_healthy_stream_uses_a_held_quote_however_old_it_is() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["AAA"]);
    tradable(
        &rig.book,
        "AAA",
        "99.50",
        "100.00",
        released - ChronoDuration::hours(3),
        1,
    );
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    settle().await;
    let trade = rig.store.trade();
    assert_eq!(
        trade.status(),
        TradeStatus::Open,
        "a thin share stays current"
    );
    assert_eq!(trade.entry_quote_source.as_deref(), Some("stream"));
    assert_eq!(trade.entry_quote_age_ms, Some(3 * 3600 * 1000));
    assert!(rig.quotes.times().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_missing_heartbeat_makes_the_stream_unhealthy_so_the_next_price_is_requested() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["AAA"]);
    tradable(&rig.book, "AAA", "99.50", "100.00", released, 1);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        rig.book.is_healthy(),
        "exactly the timeout is still healthy"
    );
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(!rig.book.is_healthy());
    hand_off(
        &rig,
        signal(
            1,
            "AAA",
            "XSTO",
            DecisionKind::Buy,
            released + ChronoDuration::seconds(3),
        ),
    );
    settle().await;
    assert_eq!(rig.quotes.times().len(), 1, "requested");
    assert_eq!(
        rig.store.trade().entry_quote_source.as_deref(),
        Some("request")
    );
}

#[tokio::test(start_paused = true)]
async fn saxo_connected_false_in_the_heartbeat_makes_the_stream_unhealthy() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["AAA"]);
    tradable(&rig.book, "AAA", "99.50", "100.00", released, 1);
    rig.book.on_heartbeat(false);
    assert!(!rig.book.is_healthy());
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    settle().await;
    assert_eq!(rig.quotes.times().len(), 1);
    assert_eq!(
        rig.store.trade().entry_quote_source.as_deref(),
        Some("request")
    );
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_dies_while_the_entry_waits_hands_over_to_a_request() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["AAA"]);
    held(
        &rig.book,
        "AAA",
        "99.00",
        "101.00",
        "Indicative",
        released,
        1,
    );
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(
        rig.quotes.times().len(),
        1,
        "one request once the stream lapsed"
    );
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::Open);
    assert_eq!(trade.entry_quote_source.as_deref(), Some("request"));
}

#[tokio::test(start_paused = true)]
async fn at_the_exit_a_dead_stream_and_an_old_fallback_quote_leave_no_result() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["AAA"]);
    tradable(&rig.book, "AAA", "99.50", "100.00", released, 1);
    let alive = keep_alive(&rig.book);
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    settle().await;
    tokio::time::sleep(Duration::from_secs(20 * 60)).await;
    alive.abort();
    rig.quotes.set_age(Duration::from_secs(2 * 3600));
    tokio::time::sleep(Duration::from_secs(20 * 60)).await;
    let trade = rig.store.trade();
    assert_eq!(trade.status(), TradeStatus::ExitQuoteTooOld);
    assert_eq!(trade.pnl_pct, None);
    assert_eq!(trade.exit_at, None);
    assert_eq!(trade.exit_bid, None);
    assert_eq!(trade.exit_quote_source.as_deref(), Some("request"));
    assert_eq!(trade.exit_quote_age_ms, Some(2 * 3600 * 1000));
    let observations = rig.store.observations_of(trade.id);
    let status = |h: &str| {
        observations
            .iter()
            .find(|o| o.horizon == h)
            .map(|o| (o.status.clone(), o.quote_source.clone()))
            .expect("observation")
    };
    assert_eq!(status("1m"), ("ok".into(), Some("stream".into())));
    assert_eq!(status("15m"), ("ok".into(), Some("stream".into())));
    assert_eq!(
        status("hold"),
        ("quote_too_old".into(), Some("request".into()))
    );
    assert_eq!(
        rig.sink.messages().last().expect("emitted").1.status,
        "exit_quote_too_old"
    );
}

#[tokio::test(start_paused = true)]
async fn an_unscheduled_release_joins_the_watch_list_and_takes_its_entry_by_request() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["BBB"]);
    let alive = keep_alive(&rig.book);
    tradable(&rig.book, "AAA", "50.00", "51.00", released, 1);
    assert!(!rig.book.is_watched(&key("AAA", "XSTO")));
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    assert!(
        rig.book.is_watched(&key("AAA", "XSTO")),
        "added at once, before the task runs"
    );
    settle().await;
    let trade = rig.store.trade();
    assert_eq!(rig.quotes.times(), [released], "the entry is requested");
    assert_eq!(
        trade.entry_ask,
        Some(dec("100.00")),
        "not the unwatched book price"
    );
    assert_eq!(trade.entry_quote_source.as_deref(), Some("request"));
    tokio::time::sleep(Duration::from_secs(40 * 60)).await;
    assert_eq!(rig.store.trade().status(), TradeStatus::Closed);
    tokio::time::sleep(Duration::from_secs(7 * 3600)).await;
    alive.abort();
    assert!(
        !rig.book.is_watched(&key("AAA", "XSTO")),
        "removed once its trade is over"
    );
    assert!(rig.book.is_watched(&key("BBB", "XSTO")));
}

#[tokio::test(start_paused = true)]
async fn an_older_sequence_number_does_not_replace_the_quote_the_entry_reads() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    scheduled(&rig, &["AAA"]);
    tradable(&rig.book, "AAA", "99.50", "100.00", released, 5);
    tradable(&rig.book, "AAA", "89.50", "90.00", released - ms(500), 4);
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    settle().await;
    assert_eq!(rig.store.trade().entry_ask, Some(dec("100.00")));
}

#[tokio::test(start_paused = true)]
async fn a_resumed_trade_keeps_its_instrument_on_the_watch_list() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, flat("101.50", "100.00"));
    up(&rig.book);
    let alive = keep_alive(&rig.book);
    tradable(&rig.book, "AAA", "99.50", "100.00", released, 1);
    scheduled(&rig, &["AAA"]);
    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    settle().await;
    rig.book.set_scheduled(Default::default());
    assert!(
        rig.book.is_watched(&key("AAA", "XSTO")),
        "an open trade pins its instrument after the target is gone"
    );
    tokio::time::sleep(Duration::from_secs(8 * 3600)).await;
    alive.abort();
    assert!(!rig.book.is_watched(&key("AAA", "XSTO")));
}
