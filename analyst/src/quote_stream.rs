//! The background task that keeps the [`QuoteBook`] current: it connects to
//! `saxo-stream`'s `/quotes/stream`, sends the watch list on connect and on
//! every change, and applies the quotes and heartbeats it receives.
//!
//! Read-only: the only message this client sends is the watch list. It
//! reconnects with a backoff (100 ms doubling to 5 s), resends the watch list
//! on every connection, and starts a new session on each one so `saxo-stream`'s
//! restarting sequence numbers are accepted.

use std::collections::BTreeSet;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::metrics;
use crate::quote_book::{key, Pair, QuoteBook};
use crate::quotes::Quote;

/// First wait before reconnecting.
pub const BACKOFF_MIN: Duration = Duration::from_millis(100);
/// Longest wait before reconnecting.
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// How long opening the connection may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A connection that has been silent this long is closed and reopened, so a
/// half-open socket cannot hold the stream "connected" without data.
const MIN_SILENCE: Duration = Duration::from_secs(10);
/// How often the health gauge is refreshed and silence is checked.
const TICK: Duration = Duration::from_millis(250);

/// The stream client's timings.
#[derive(Debug, Clone, Copy)]
pub struct StreamTimings {
    /// First wait before reconnecting.
    pub backoff_min: Duration,
    /// Longest wait before reconnecting.
    pub backoff_max: Duration,
    /// Silence after which a connection is dropped.
    pub max_silence: Duration,
}

impl StreamTimings {
    /// The defaults for a heartbeat timeout of `heartbeat_timeout`.
    pub fn new(heartbeat_timeout: Duration) -> StreamTimings {
        StreamTimings {
            backoff_min: BACKOFF_MIN,
            backoff_max: BACKOFF_MAX,
            max_silence: MIN_SILENCE.max(heartbeat_timeout * 3),
        }
    }
}

#[derive(Serialize)]
struct WatchPair<'a> {
    ticker: &'a str,
    mic: &'a str,
}

#[derive(Serialize)]
struct WatchMessage<'a> {
    watch: Vec<WatchPair<'a>>,
}

/// The JSON of a watch-list message for `pairs`.
pub fn watch_message(pairs: &BTreeSet<Pair>) -> String {
    let message = WatchMessage {
        watch: pairs
            .iter()
            .map(|(ticker, mic)| WatchPair { ticker, mic })
            .collect(),
    };
    serde_json::to_string(&message).expect("a watch list always serializes")
}

#[derive(Deserialize)]
struct QuoteMessage {
    ticker: String,
    mic: String,
    bid: Option<String>,
    ask: Option<String>,
    bid_type: Option<String>,
    ask_type: Option<String>,
    delayed_minutes: Option<i64>,
    currency: Option<String>,
    saxo_time: Option<String>,
    seq: u64,
}

#[derive(Deserialize)]
struct HeartbeatMessage {
    saxo_connected: bool,
}

#[derive(Deserialize)]
struct ErrorMessage {
    pair: Option<ErrorPair>,
    reason: String,
}

#[derive(Deserialize)]
struct ErrorPair {
    ticker: String,
    mic: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ServerMessage {
    Quote(QuoteMessage),
    Heartbeat(HeartbeatMessage),
    Error(ErrorMessage),
}

fn decimal(name: &str, text: Option<String>) -> Result<Option<Decimal>, String> {
    text.map(|t| Decimal::from_str(&t).map_err(|e| format!("{name} `{t}`: {e}")))
        .transpose()
}

fn to_quote(message: QuoteMessage) -> Result<(Pair, Quote, u64), String> {
    let pair = key(&message.ticker, &message.mic);
    let quote = Quote {
        bid: decimal("bid", message.bid)?,
        ask: decimal("ask", message.ask)?,
        bid_type: message.bid_type,
        ask_type: message.ask_type,
        delayed_minutes: message.delayed_minutes,
        currency: message.currency,
        saxo_last_updated: message.saxo_time,
    };
    Ok((pair, quote, message.seq))
}

/// Applies one text message from `saxo-stream` to `book`.
pub fn handle_message(book: &QuoteBook, text: &str) {
    match serde_json::from_str::<ServerMessage>(text) {
        Ok(ServerMessage::Quote(message)) => match to_quote(message) {
            Ok((pair, quote, seq)) => {
                book.apply(&pair, quote, seq);
            }
            Err(error) => warn!(%error, "a streamed quote had an unreadable price, dropped"),
        },
        Ok(ServerMessage::Heartbeat(heartbeat)) => book.on_heartbeat(heartbeat.saxo_connected),
        Ok(ServerMessage::Error(error)) => {
            let pair = error
                .pair
                .map(|p| format!("{}:{}", p.ticker, p.mic))
                .unwrap_or_default();
            warn!(pair, reason = %error.reason, "saxo-stream reported a problem with a pair");
        }
        Err(error) => debug!(%error, "unrecognised message from saxo-stream"),
    }
}

/// Runs the stream client until `shutdown` completes.
pub async fn run_quote_stream(
    url: String,
    book: Arc<QuoteBook>,
    timings: StreamTimings,
    shutdown: impl Future<Output = ()>,
) {
    let mut shutdown = std::pin::pin!(shutdown);
    let mut backoff = timings.backoff_min;
    let mut first = true;
    loop {
        if !first {
            metrics::quote_stream_reconnect();
        }
        first = false;
        let outcome = tokio::select! {
            () = &mut shutdown => {
                book.on_disconnected();
                metrics::quote_stream_connected(false);
                return;
            }
            outcome = session(&url, &book, timings) => outcome,
        };
        book.on_disconnected();
        metrics::quote_stream_connected(false);
        match outcome {
            SessionEnd::Delivered => backoff = timings.backoff_min,
            SessionEnd::Empty => {}
        }
        tokio::select! {
            () = &mut shutdown => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(timings.backoff_max);
    }
}

enum SessionEnd {
    Delivered,
    Empty,
}

async fn session(url: &str, book: &Arc<QuoteBook>, timings: StreamTimings) -> SessionEnd {
    let connect = tokio_tungstenite::connect_async(url);
    let socket = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(error)) => {
            warn!(url, %error, "cannot connect to the quote stream");
            return SessionEnd::Empty;
        }
        Err(_) => {
            warn!(url, "connecting to the quote stream timed out");
            return SessionEnd::Empty;
        }
    };
    info!(url, "quote stream connected");
    book.on_connected();
    let (mut sink, mut stream) = socket.split();
    let mut changes = book.watch_changes();
    let mut delivered = false;
    let list = changes.borrow_and_update().clone();
    if sink
        .send(Message::Text(watch_message(&list)))
        .await
        .is_err()
    {
        return SessionEnd::Empty;
    }
    let mut last_rx = Instant::now();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    last_rx = Instant::now();
                    delivered = true;
                    handle_message(book, &text);
                    metrics::quote_stream_connected(book.is_healthy());
                }
                Some(Ok(Message::Close(frame))) => {
                    info!(?frame, "quote stream closed by saxo-stream");
                    break;
                }
                Some(Ok(_)) => last_rx = Instant::now(),
                Some(Err(error)) => {
                    warn!(%error, "quote stream failed");
                    break;
                }
                None => {
                    info!("quote stream ended");
                    break;
                }
            },
            changed = changes.changed() => {
                if changed.is_err() {
                    break;
                }
                let list = changes.borrow_and_update().clone();
                if sink.send(Message::Text(watch_message(&list))).await.is_err() {
                    break;
                }
            },
            _ = tick.tick() => {
                metrics::quote_stream_connected(book.is_healthy());
                if last_rx.elapsed() > timings.max_silence {
                    warn!("quote stream silent for too long, reconnecting");
                    break;
                }
            }
        }
    }
    if delivered {
        SessionEnd::Delivered
    } else {
        SessionEnd::Empty
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::{TimeZone, Utc};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    use super::*;
    use crate::clock::SystemClock;

    fn pair(t: &str) -> Pair {
        key(t, "XSTO")
    }

    fn book() -> Arc<QuoteBook> {
        QuoteBook::new(Arc::new(SystemClock), Duration::from_secs(3))
    }

    fn quote_json(ticker: &str, ask: &str, seq: u64) -> String {
        serde_json::json!({
            "type": "quote", "ticker": ticker, "mic": "XSTO",
            "bid": "99.50", "ask": ask, "bid_size": "100", "ask_size": "100",
            "bid_type": "Tradable", "ask_type": "Tradable", "market_state": "Open",
            "delayed_minutes": 0, "currency": "SEK",
            "saxo_time": "2026-11-10T08:00:00.123Z",
            "received_at": "2026-11-10T08:00:00.200Z", "seq": seq
        })
        .to_string()
    }

    fn heartbeat_json(connected: bool) -> String {
        serde_json::json!({"type": "heartbeat", "at": "2026-11-10T08:00:00Z",
            "saxo_connected": connected, "unresolved": []})
        .to_string()
    }

    #[test]
    fn the_watch_message_lists_every_pair() {
        let list: BTreeSet<Pair> = [pair("AAA"), pair("BBB")].into();
        assert_eq!(
            watch_message(&list),
            r#"{"watch":[{"ticker":"AAA","mic":"XSTO"},{"ticker":"BBB","mic":"XSTO"}]}"#
        );
        assert_eq!(watch_message(&BTreeSet::new()), r#"{"watch":[]}"#);
    }

    #[test]
    fn quotes_and_heartbeats_are_applied_and_junk_is_ignored() {
        let book = book();
        book.on_connected();
        handle_message(&book, &quote_json("aaa", "100.00", 3));
        let held = book.current(&pair("AAA")).expect("held");
        assert_eq!(held.quote.ask, Some("100.00".parse().unwrap()));
        assert_eq!(held.quote.bid, Some("99.50".parse().unwrap()));
        assert_eq!(held.quote.ask_type.as_deref(), Some("Tradable"));
        assert_eq!(held.quote.delayed_minutes, Some(0));
        assert_eq!(held.seq, 3);
        assert_eq!(
            held.saxo_time,
            Some(
                Utc.with_ymd_and_hms(2026, 11, 10, 8, 0, 0).unwrap()
                    + chrono::Duration::milliseconds(123)
            )
        );
        handle_message(&book, &quote_json("AAA", "99.00", 2));
        assert_eq!(book.current(&pair("AAA")).unwrap().seq, 3);
        handle_message(&book, "not json");
        handle_message(&book, r#"{"type":"mystery"}"#);
        handle_message(
            &book,
            r#"{"type":"error","pair":{"ticker":"X","mic":"XSTO"},"reason":"nope"}"#,
        );
        assert!(!book.is_healthy());
        handle_message(&book, &heartbeat_json(true));
        assert!(book.is_healthy());
        handle_message(&book, &heartbeat_json(false));
        assert!(!book.is_healthy());
    }

    #[test]
    fn an_unreadable_price_drops_the_quote_instead_of_guessing() {
        let book = book();
        handle_message(&book, &quote_json("AAA", "abc", 1));
        assert_eq!(book.current(&pair("AAA")), None);
    }

    enum ServerCommand {
        Send(String),
        Drop,
    }

    struct FakeServer {
        url: String,
        received: Arc<Mutex<Vec<String>>>,
        connections: Arc<Mutex<usize>>,
        commands: mpsc::UnboundedSender<ServerCommand>,
    }

    async fn fake_server() -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}/quotes/stream", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(0usize));
        let (commands, mut rx) = mpsc::unbounded_channel();
        let (seen, count) = (received.clone(), connections.clone());
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                    continue;
                };
                *count.lock().unwrap() += 1;
                loop {
                    tokio::select! {
                        frame = ws.next() => match frame {
                            Some(Ok(Message::Text(text))) => seen.lock().unwrap().push(text),
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                        command = rx.recv() => match command {
                            Some(ServerCommand::Send(text)) => {
                                let _ = ws.send(Message::Text(text)).await;
                            }
                            Some(ServerCommand::Drop) => break,
                            None => return,
                        },
                    }
                }
            }
        });
        FakeServer {
            url,
            received,
            connections,
            commands,
        }
    }

    fn fast() -> StreamTimings {
        StreamTimings {
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(80),
            max_silence: Duration::from_secs(30),
        }
    }

    async fn eventually(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn the_watch_list_is_sent_on_connect_and_on_every_change() {
        let server = fake_server().await;
        let book = book();
        book.set_scheduled([pair("AAA")].into());
        let task = tokio::spawn(run_quote_stream(
            server.url.clone(),
            book.clone(),
            fast(),
            std::future::pending(),
        ));
        eventually("the first watch list", || {
            server.received.lock().unwrap().len() == 1
        })
        .await;
        assert_eq!(
            server.received.lock().unwrap()[0],
            r#"{"watch":[{"ticker":"AAA","mic":"XSTO"}]}"#
        );
        let pin = book.pin(pair("BBB"));
        eventually("the second watch list", || {
            server.received.lock().unwrap().len() == 2
        })
        .await;
        assert_eq!(
            server.received.lock().unwrap()[1],
            r#"{"watch":[{"ticker":"AAA","mic":"XSTO"},{"ticker":"BBB","mic":"XSTO"}]}"#
        );
        drop(pin);
        eventually("the third watch list", || {
            server.received.lock().unwrap().len() == 3
        })
        .await;
        task.abort();
    }

    #[tokio::test]
    async fn streamed_quotes_and_heartbeats_reach_the_book_and_health_follows() {
        let server = fake_server().await;
        let book = book();
        let task = tokio::spawn(run_quote_stream(
            server.url.clone(),
            book.clone(),
            fast(),
            std::future::pending(),
        ));
        eventually("connected", || *server.connections.lock().unwrap() == 1).await;
        assert!(!book.is_healthy());
        server
            .commands
            .send(ServerCommand::Send(heartbeat_json(true)))
            .unwrap();
        server
            .commands
            .send(ServerCommand::Send(quote_json("AAA", "100.00", 1)))
            .unwrap();
        eventually("a quote", || book.current(&pair("AAA")).is_some()).await;
        assert!(book.is_healthy());
        server
            .commands
            .send(ServerCommand::Send(heartbeat_json(false)))
            .unwrap();
        eventually("saxo down", || !book.is_healthy()).await;
        task.abort();
    }

    #[tokio::test]
    async fn after_a_dropped_connection_it_reconnects_and_resends_the_watch_list() {
        let server = fake_server().await;
        let book = book();
        book.set_scheduled([pair("AAA")].into());
        let task = tokio::spawn(run_quote_stream(
            server.url.clone(),
            book.clone(),
            fast(),
            std::future::pending(),
        ));
        eventually("connected", || *server.connections.lock().unwrap() == 1).await;
        server
            .commands
            .send(ServerCommand::Send(heartbeat_json(true)))
            .unwrap();
        server
            .commands
            .send(ServerCommand::Send(quote_json("AAA", "100.00", 500)))
            .unwrap();
        eventually("healthy", || book.is_healthy()).await;
        server.commands.send(ServerCommand::Drop).unwrap();
        eventually("unhealthy after the drop", || !book.is_healthy()).await;
        eventually("reconnected", || *server.connections.lock().unwrap() == 2).await;
        eventually("the list resent", || {
            server.received.lock().unwrap().len() == 2
        })
        .await;
        let sent = server.received.lock().unwrap().clone();
        assert_eq!(sent[0], sent[1]);
        server
            .commands
            .send(ServerCommand::Send(quote_json("AAA", "98.00", 1)))
            .unwrap();
        eventually("the restarted sequence is accepted", || {
            book.current(&pair("AAA")).map(|h| h.seq) == Some(1)
        })
        .await;
        task.abort();
    }

    #[tokio::test]
    async fn it_keeps_trying_while_nothing_listens() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/quotes/stream", listener.local_addr().unwrap());
        drop(listener);
        let book = book();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(run_quote_stream(url, book.clone(), fast(), async {
            let _ = stop_rx.await;
        }));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!book.is_healthy());
        stop_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("stops on shutdown")
            .unwrap();
    }
}
