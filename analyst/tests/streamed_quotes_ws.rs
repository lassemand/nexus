//! The stream client, the quote book and the trade runner together against a fake
//! `saxo-stream` WebSocket, in real time: the watch list the server receives, and
//! an entry priced from the streamed quote without a request.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use analyst::quote_book::key;
use analyst::{run_quote_stream, DecisionKind, StreamTimings, TradeStatus};
use common::trading::{hand_off, quote, rig, signal, sto};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

struct FakeSaxoStream {
    url: String,
    received: Arc<Mutex<Vec<String>>>,
    send: mpsc::UnboundedSender<String>,
}

async fn fake_saxo_stream() -> FakeSaxoStream {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!(
        "ws://{}/quotes/stream",
        listener.local_addr().expect("addr")
    );
    let received = Arc::new(Mutex::new(Vec::new()));
    let seen = received.clone();
    let (send, mut outgoing) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let Ok((tcp, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
            return;
        };
        loop {
            tokio::select! {
                frame = ws.next() => match frame {
                    Some(Ok(Message::Text(text))) => seen.lock().expect("lock").push(text),
                    Some(Ok(_)) => {}
                    _ => return,
                },
                text = outgoing.recv() => match text {
                    Some(text) => { let _ = ws.send(Message::Text(text)).await; }
                    None => return,
                },
            }
        }
    });
    FakeSaxoStream {
        url,
        received,
        send,
    }
}

async fn eventually(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn quote_json(ticker: &str, ask: &str, saxo_time: &str, seq: u64) -> String {
    serde_json::json!({
        "type": "quote", "ticker": ticker, "mic": "XSTO",
        "bid": "99.50", "ask": ask, "bid_size": "100", "ask_size": "100",
        "bid_type": "Tradable", "ask_type": "Tradable", "market_state": "Open",
        "delayed_minutes": 0, "currency": "SEK", "saxo_time": saxo_time,
        "received_at": saxo_time, "seq": seq
    })
    .to_string()
}

#[tokio::test]
async fn a_streamed_quote_prices_the_entry_and_an_unscheduled_pair_reaches_the_server() {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, |_| {
        Ok(quote("101.50", "100.00"))
    });
    let server = fake_saxo_stream().await;
    rig.book.set_scheduled([key("AAA", "XSTO")].into());
    let client = tokio::spawn(run_quote_stream(
        server.url.clone(),
        rig.book.clone(),
        StreamTimings::new(Duration::from_secs(3)),
        std::future::pending(),
    ));
    eventually("the first watch list", || {
        server.received.lock().expect("lock").len() == 1
    })
    .await;
    assert_eq!(
        server.received.lock().expect("lock")[0],
        r#"{"watch":[{"ticker":"AAA","mic":"XSTO"}]}"#
    );
    server
        .send
        .send(
            serde_json::json!({"type":"heartbeat","at":"2026-11-10T10:00:00Z",
                "saxo_connected": true, "unresolved": []})
            .to_string(),
        )
        .expect("send");
    server
        .send
        .send(quote_json("AAA", "100.00", "2026-11-10T09:59:59Z", 1))
        .expect("send");
    eventually("the quote in the book", || {
        rig.book.is_healthy() && rig.book.current(&key("AAA", "XSTO")).is_some()
    })
    .await;

    hand_off(&rig, signal(1, "AAA", "XSTO", DecisionKind::Buy, released));
    hand_off(&rig, signal(2, "BBB", "XSTO", DecisionKind::Buy, released));
    eventually("the second watch list", || {
        server.received.lock().expect("lock").len() == 2
    })
    .await;
    assert_eq!(
        server.received.lock().expect("lock")[1],
        r#"{"watch":[{"ticker":"AAA","mic":"XSTO"},{"ticker":"BBB","mic":"XSTO"}]}"#,
        "the unscheduled pair is added at once"
    );
    eventually("both trades open", || {
        rig.store
            .trades()
            .iter()
            .filter(|t| t.status() == TradeStatus::Open)
            .count()
            == 2
    })
    .await;
    let trades = rig.store.trades();
    let a = trades.iter().find(|t| t.ticker == "AAA").expect("AAA");
    let b = trades.iter().find(|t| t.ticker == "BBB").expect("BBB");
    assert_eq!(a.entry_quote_source.as_deref(), Some("stream"));
    assert_eq!(a.entry_at, Some(released));
    assert_eq!(b.entry_quote_source.as_deref(), Some("request"));
    assert_eq!(
        rig.quotes.times().len(),
        1,
        "only the unscheduled entry was requested"
    );
    client.abort();
}
