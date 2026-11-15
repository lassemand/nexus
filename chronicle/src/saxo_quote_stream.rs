//! saxo-stream's streaming quotes (NEX-270): Saxo info prices for the
//! instruments clients watch, pushed to them over `GET /quotes/stream`.
//!
//! # Shape
//!
//! One task, the hub, owns the Saxo connection, the subscriptions and the
//! merged quote per Uic. Nothing else touches them, so no lock is held across
//! an await. Everything slow (connect, subscribe, delete, re-authorize) runs in
//! spawned tasks that report back to the hub through a channel; the hub itself
//! only decodes frames, merges deltas, serializes each quote once and
//! `try_send`s it into every watching client's bounded queue. A client's
//! WebSocket writer drains that queue on its own task, so a slow client never
//! slows Saxo reading or the other clients.
//!
//! # Saxo side
//!
//! - Connects while the union of the clients' watch lists is non-empty, and
//!   disconnects after it has been empty for [`StreamSettings::idle_disconnect`].
//! - A watch-list change is applied after [`StreamSettings::debounce`]: new Uics
//!   are subscribed (several per subscription, up to
//!   [`StreamSettings::max_uics_per_subscription`]); a subscription whose Uics
//!   no client needs is deleted, and one that is partly needed is replaced
//!   (`ReplaceReferenceId`) by one with the rest.
//! - A subscription's snapshot replaces the stored quotes; every later message
//!   is a delta merged into them ([`alpha::saxo::StreamQuote::apply`]). Deltas
//!   that arrive before their subscription's snapshot are buffered and applied
//!   after it.
//! - While at least one subscription is active, no message for Saxo's
//!   `InactivityTimeout` plus [`StreamSettings::heartbeat_timeout`] (heartbeats
//!   are per subscription, so a connection with none is never declared dead), a
//!   `_disconnect`, or a socket error ends the connection; it is reopened after a backoff (1 s
//!   doubling to 30 s) under a new context id, everything is subscribed again
//!   and each snapshot replaces the stored quote. `seq` is per instrument and
//!   never restarts.
//! - A failed subscribe is retried per Uic with exponential backoff, at least
//!   Saxo's `Retry-After`; a 401 is an auth problem (the token is not
//!   used again until it rotates or `backoff_max` passes), never a refusal; a failed replacement deletes the replaced
//!   subscription; `SubscriptionPermanentlyDisabled` is a refusal until the
//!   next reconnect.
//! - `_resetsubscriptions` re-creates the named subscriptions (all when none is
//!   named) under new reference ids.
//! - When saxo-stream rotates the access token, the hub `PUT`s it to the
//!   authorize URL. It never authenticates.
//!
//! # Client side
//!
//! See [`router`] for the protocol. Without a token, or while Saxo is down,
//! clients stay connected and heartbeats say `saxo_connected: false`.

use alpha::saxo::stream::{self as saxo, Control, InfoPriceDelta, SaxoSocket, Subscribed};
use alpha::saxo::{InfoPriceStreamClient, StreamError, StreamQuote};
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::{SinkExt, StreamExt};
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as SaxoMessage;
use tracing::{debug, error, info, warn};

use crate::saxo_quotes::{self, TokenSlot, UicMap};

/// Longest wait taken from a Saxo `Retry-After`.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);

/// A listing: upper-cased ticker and MIC ([`saxo_quotes::key`]).
pub type Pair = (String, String);

/// Buckets (seconds) of `saxo_quote_stream_fanout_seconds`.
pub const FANOUT_BUCKETS: &[f64] = &[
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5,
];

/// Buckets (seconds) of `saxo_quote_stream_saxo_lag_seconds`.
pub const SAXO_LAG_BUCKETS: &[f64] = &[
    0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// Every `reason` of `saxo_quote_stream_client_closed_total`.
pub const CLOSE_REASONS: [&str; 3] = [REASON_CLIENT, REASON_SLOW, REASON_SEND_FAILED];
/// The client went away.
pub const REASON_CLIENT: &str = "client";
/// The client's queue stayed full for [`StreamSettings::slow_client_grace`].
pub const REASON_SLOW: &str = "slow_consumer";
/// Writing to the client's socket failed.
pub const REASON_SEND_FAILED: &str = "send_failed";

/// Timings and limits of the stream.
#[derive(Debug, Clone)]
pub struct StreamSettings {
    /// No Saxo message (heartbeats included) for this long means the
    /// connection is dead (`SAXO_STREAM_HEARTBEAT_TIMEOUT`).
    pub heartbeat_timeout: Duration,
    /// Capacity of each client's queue (`QUOTE_STREAM_CLIENT_QUEUE`).
    pub client_queue: usize,
    /// Most Uics put in one Saxo subscription
    /// (`SAXO_STREAM_MAX_UICS_PER_SUBSCRIPTION`).
    pub max_uics_per_subscription: usize,
    /// Wait after a watch-list change before subscriptions are changed.
    pub debounce: Duration,
    /// How long the union of watch lists stays empty before Saxo is
    /// disconnected.
    pub idle_disconnect: Duration,
    /// First wait before reconnecting.
    pub backoff_min: Duration,
    /// Longest wait before reconnecting.
    pub backoff_max: Duration,
    /// Interval of client heartbeats and of the hub's housekeeping.
    pub tick: Duration,
    /// How long a client's queue may stay full before the client is closed.
    pub slow_client_grace: Duration,
    /// How long a watched pair may stay unresolved before the client gets an
    /// error for it.
    pub unresolved_error_after: Duration,
    /// How often an unresolved pair is handed to the background resolver again.
    pub resolve_retry: Duration,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            heartbeat_timeout: Duration::from_secs(15),
            client_queue: 1024,
            max_uics_per_subscription: 100,
            debounce: Duration::from_millis(200),
            idle_disconnect: Duration::from_secs(5 * 60),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            tick: Duration::from_secs(1),
            slow_client_grace: Duration::from_secs(5),
            unresolved_error_after: Duration::from_secs(30),
            resolve_retry: Duration::from_secs(60),
        }
    }
}

/// What the hub needs from saxo-stream.
pub struct StreamDeps {
    /// Saxo's streaming endpoints.
    pub client: InfoPriceStreamClient,
    /// saxo-stream's access token, once it has one.
    pub token: TokenSlot,
    /// Resolved Uics, shared with `GET /quote`.
    pub uics: UicMap,
    /// The background resolver's queue, shared with `GET /quote`.
    pub unknown: mpsc::Sender<Pair>,
}

/// One message for a client, serialized once and shared by every client.
#[derive(Debug, Clone)]
pub struct Outgoing {
    /// The JSON text.
    pub text: Utf8Bytes,
    /// When the Saxo frame behind it arrived; `None` for heartbeats, errors and
    /// snapshots sent on watch.
    pub received: Option<Instant>,
}

/// A joined client: its queue and the reason it is closed with, if the hub
/// closes it.
pub struct Subscriber {
    /// The client's id in the hub.
    pub id: u64,
    /// The client's queue.
    pub rx: mpsc::Receiver<Outgoing>,
    /// Fires with a reason when the hub closes the client.
    pub close: oneshot::Receiver<&'static str>,
}

enum Command {
    Join {
        id: u64,
        tx: mpsc::Sender<Outgoing>,
        close: oneshot::Sender<&'static str>,
    },
    Watch {
        id: u64,
        pairs: Vec<Pair>,
    },
    Leave {
        id: u64,
        reason: &'static str,
    },
    Invalid {
        id: u64,
        reason: String,
    },
}

/// The hub's address. Cheap to clone.
#[derive(Clone)]
pub struct QuoteStreamHandle {
    cmd: mpsc::Sender<Command>,
    next_id: Arc<AtomicU64>,
    queue: usize,
}

impl QuoteStreamHandle {
    /// Joins a client with the configured queue capacity.
    pub async fn join(&self) -> Option<Subscriber> {
        self.join_with_queue(self.queue).await
    }

    /// Joins a client whose queue holds `capacity` messages.
    pub async fn join_with_queue(&self, capacity: usize) -> Option<Subscriber> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let (close_tx, close) = oneshot::channel();
        self.cmd
            .send(Command::Join {
                id,
                tx,
                close: close_tx,
            })
            .await
            .ok()?;
        Some(Subscriber { id, rx, close })
    }

    /// Replaces client `id`'s watch list.
    pub async fn watch(&self, id: u64, pairs: Vec<Pair>) {
        let _ = self.cmd.send(Command::Watch { id, pairs }).await;
    }

    /// Removes client `id`, counting it under `reason`.
    pub async fn leave(&self, id: u64, reason: &'static str) {
        let _ = self.cmd.send(Command::Leave { id, reason }).await;
    }

    /// Tells client `id` its last message was not understood.
    async fn invalid(&self, id: u64, reason: String) {
        let _ = self.cmd.send(Command::Invalid { id, reason }).await;
    }
}

/// Registers the stream metrics at zero, with their descriptions.
pub fn describe() {
    for reason in CLOSE_REASONS {
        counter!("saxo_quote_stream_client_closed_total", "reason" => reason).absolute(0);
    }
    counter!("saxo_quote_stream_reconnects_total").absolute(0);
    counter!("saxo_quote_stream_updates_total").absolute(0);
    counter!("saxo_quote_stream_reauthorize_failures_total").absolute(0);
    gauge!("saxo_quote_stream_connected").set(0.0);
    gauge!("saxo_quote_stream_watched_instruments").set(0.0);
    gauge!("saxo_quote_stream_clients").set(0.0);
    describe_gauge!(
        "saxo_quote_stream_connected",
        "1 while the Saxo streaming connection is up and every subscription has its snapshot."
    );
    describe_counter!(
        "saxo_quote_stream_reconnects_total",
        "Saxo streaming connections lost and reopened (error, timeout, _disconnect)."
    );
    describe_counter!(
        "saxo_quote_stream_updates_total",
        "Instrument updates (snapshot entries and deltas) merged from Saxo."
    );
    describe_counter!(
        "saxo_quote_stream_reauthorize_failures_total",
        "Failed PUTs of a rotated access token to Saxo's streaming authorize URL."
    );
    describe_gauge!(
        "saxo_quote_stream_watched_instruments",
        "Pairs in the union of every client's watch list."
    );
    describe_gauge!(
        "saxo_quote_stream_clients",
        "Clients connected to /quotes/stream."
    );
    describe_counter!(
        "saxo_quote_stream_client_closed_total",
        "Clients that left /quotes/stream, by reason."
    );
    describe_histogram!(
        "saxo_quote_stream_fanout_seconds",
        "Time from a Saxo frame received to its quote written to a client's socket."
    );
    describe_histogram!(
        "saxo_quote_stream_saxo_lag_seconds",
        "received_at minus Saxo's LastUpdated, per update."
    );
}

/// Starts the hub and returns its handle.
pub fn spawn(deps: StreamDeps, settings: StreamSettings) -> QuoteStreamHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let handle = QuoteStreamHandle {
        cmd: cmd_tx,
        next_id: Arc::new(AtomicU64::new(1)),
        queue: settings.client_queue,
    };
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let hub = Hub::new(deps, settings, ev_tx);
    tokio::spawn(hub.run(cmd_rx, ev_rx));
    handle
}

enum Event {
    Connected {
        ctx: String,
        token: String,
        result: Result<Box<SaxoSocket>, StreamError>,
    },
    Subscribed {
        ctx: String,
        reference: String,
        replace: Option<String>,
        token: String,
        result: Result<Subscribed, StreamError>,
        at: Instant,
        at_utc: DateTime<Utc>,
    },
    Reauthorized {
        ctx: String,
        token: String,
        ok: bool,
    },
}

struct Client {
    tx: mpsc::Sender<Outgoing>,
    close: Option<oneshot::Sender<&'static str>>,
    watch: HashSet<Pair>,
    pending: HashMap<Pair, Outgoing>,
    errors: VecDeque<Outgoing>,
    full_since: Option<Instant>,
}

/// Most error messages kept for a client whose queue is full.
const MAX_QUEUED_ERRORS: usize = 32;

impl Client {
    /// Queues `out`; when the queue is full, keeps only the latest message per
    /// pair until there is room. `false` once the client is gone.
    fn deliver(&mut self, pair: Option<&Pair>, out: Outgoing) -> bool {
        if !self.pending.is_empty() || !self.errors.is_empty() {
            if let Some(p) = pair {
                self.pending.insert(p.clone(), out);
            }
            return self.flush();
        }
        match self.tx.try_send(out) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(out)) => {
                if let Some(p) = pair {
                    self.pending.insert(p.clone(), out);
                }
                self.full_since.get_or_insert_with(Instant::now);
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Queues an error message. Errors are never coalesced or dropped for a
    /// full queue: they wait ahead of the pending quotes.
    fn deliver_error(&mut self, out: Outgoing) -> bool {
        if self.errors.len() >= MAX_QUEUED_ERRORS {
            self.errors.pop_front();
        }
        self.errors.push_back(out);
        self.flush()
    }

    /// Moves queued errors, then coalesced quotes, into the queue while there
    /// is room.
    fn flush(&mut self) -> bool {
        while let Some(out) = self.errors.pop_front() {
            match self.tx.try_send(out) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(out)) => {
                    self.errors.push_front(out);
                    self.full_since.get_or_insert_with(Instant::now);
                    return true;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return false,
            }
        }
        while let Some(pair) = self.pending.keys().next().cloned() {
            let Some(out) = self.pending.remove(&pair) else {
                break;
            };
            match self.tx.try_send(out) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(out)) => {
                    self.pending.insert(pair, out);
                    self.full_since.get_or_insert_with(Instant::now);
                    return true;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return false,
            }
        }
        if self.tx.capacity() > 0 {
            self.full_since = None;
        }
        true
    }
}

struct Entry {
    quote: StreamQuote,
    received_at: DateTime<Utc>,
}

struct Sub {
    uics: Vec<u64>,
    active: bool,
    inactivity: Option<u64>,
    buffered: Vec<(Vec<u8>, Instant, DateTime<Utc>)>,
}

struct Live {
    ctx: String,
    socket: SaxoSocket,
    token: String,
    last_msg: Instant,
    got_message: bool,
    subs: HashMap<String, Sub>,
    reauth_inflight: bool,
    reauth_failures: u32,
    reauth_next: Option<Instant>,
    awaiting_snapshot: HashSet<String>,
    resynced: bool,
}

enum Conn {
    Idle,
    Connecting { ctx: String },
    Backoff { until: Instant },
    Live(Box<Live>),
}

struct Unresolved {
    since: Instant,
    queued_at: Instant,
    notified: HashSet<u64>,
}

struct Hub {
    deps: StreamDeps,
    settings: StreamSettings,
    events: mpsc::UnboundedSender<Event>,
    clients: HashMap<u64, Client>,
    union: HashSet<Pair>,
    resolved: HashMap<Pair, u64>,
    by_uic: HashMap<u64, Vec<Pair>>,
    unresolved: HashMap<Pair, Unresolved>,
    quotes: HashMap<u64, Entry>,
    seqs: HashMap<u64, u64>,
    refused: HashSet<u64>,
    solo: HashSet<u64>,
    sub_failures: HashMap<u64, u32>,
    sub_not_before: HashMap<u64, Instant>,
    rejected_token: Option<(String, Instant)>,
    orphans: Vec<(String, String)>,
    conn: Conn,
    backoff: Duration,
    debounce_at: Option<Instant>,
    retry_at: Option<Instant>,
    empty_since: Option<Instant>,
    reconcile_after_pending: bool,
    next_ref: u64,
}

async fn next_frame(
    conn: &mut Conn,
) -> Option<Result<SaxoMessage, tokio_tungstenite::tungstenite::Error>> {
    match conn {
        Conn::Live(live) => live.socket.next().await,
        _ => std::future::pending().await,
    }
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Serialize)]
struct PairBody<'a> {
    ticker: &'a str,
    mic: &'a str,
}

#[derive(Serialize)]
struct QuoteMsg<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    ticker: &'a str,
    mic: &'a str,
    bid: Option<String>,
    ask: Option<String>,
    bid_size: Option<String>,
    ask_size: Option<String>,
    bid_type: Option<String>,
    ask_type: Option<String>,
    market_state: Option<String>,
    delayed_minutes: Option<i64>,
    currency: Option<String>,
    saxo_time: Option<String>,
    received_at: String,
    seq: u64,
}

#[derive(Serialize)]
struct HeartbeatMsg<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    at: String,
    saxo_connected: bool,
    unresolved: Vec<PairBody<'a>>,
}

#[derive(Serialize)]
struct ErrorMsg<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    pair: Option<PairBody<'a>>,
    reason: &'a str,
}

fn to_text<T: Serialize>(msg: &T) -> Utf8Bytes {
    Utf8Bytes::from(serde_json::to_string(msg).expect("stream messages always serialize"))
}

fn error_text(pair: Option<&Pair>, reason: &str) -> Utf8Bytes {
    to_text(&ErrorMsg {
        kind: "error",
        pair: pair.map(|(t, m)| PairBody { ticker: t, mic: m }),
        reason,
    })
}

fn quote_text(pair: &Pair, entry: &Entry, seq: u64) -> Utf8Bytes {
    let q = entry.quote.to_quote();
    let s = |d: Option<rust_decimal::Decimal>| d.map(|d| d.to_string());
    to_text(&QuoteMsg {
        kind: "quote",
        ticker: &pair.0,
        mic: &pair.1,
        bid: s(q.bid),
        ask: s(q.ask),
        bid_size: s(q.bid_size),
        ask_size: s(q.ask_size),
        bid_type: q.bid_type,
        ask_type: q.ask_type,
        market_state: q.market_state,
        delayed_minutes: q.delayed_minutes,
        currency: q.currency,
        saxo_time: entry.quote.last_updated().map(str::to_string),
        received_at: rfc3339(entry.received_at),
        seq,
    })
}

fn current_token(slot: &TokenSlot) -> Option<String> {
    let token = slot.get()?.lock().unwrap_or_else(|e| e.into_inner());
    (token.expires_at > Utc::now() && !token.access_token.is_empty())
        .then(|| token.access_token.clone())
}

fn record_lag(quote: &StreamQuote, received_at: DateTime<Utc>) {
    let Some(saxo_time) = quote.last_updated() else {
        return;
    };
    let Ok(t) = DateTime::parse_from_rfc3339(saxo_time) else {
        return;
    };
    if let Ok(lag) = (received_at - t.with_timezone(&Utc)).to_std() {
        if lag < Duration::from_secs(24 * 3600) {
            histogram!("saxo_quote_stream_saxo_lag_seconds").record(lag.as_secs_f64());
        }
    }
}

impl Hub {
    fn new(
        deps: StreamDeps,
        settings: StreamSettings,
        events: mpsc::UnboundedSender<Event>,
    ) -> Self {
        let backoff = settings.backoff_min;
        Self {
            deps,
            settings,
            events,
            clients: HashMap::new(),
            union: HashSet::new(),
            resolved: HashMap::new(),
            by_uic: HashMap::new(),
            unresolved: HashMap::new(),
            quotes: HashMap::new(),
            seqs: HashMap::new(),
            refused: HashSet::new(),
            solo: HashSet::new(),
            sub_failures: HashMap::new(),
            sub_not_before: HashMap::new(),
            rejected_token: None,
            orphans: Vec::new(),
            conn: Conn::Idle,
            backoff,
            debounce_at: None,
            retry_at: None,
            empty_since: None,
            reconcile_after_pending: false,
            next_ref: 1,
        }
    }

    async fn run(
        mut self,
        mut cmds: mpsc::Receiver<Command>,
        mut events: mpsc::UnboundedReceiver<Event>,
    ) {
        let mut tick = tokio::time::interval(self.settings.tick);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                frame = next_frame(&mut self.conn) => self.on_frame(frame),
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.on_command(cmd),
                    None => break,
                },
                Some(ev) = events.recv() => self.on_event(ev),
                _ = sleep_opt(self.debounce_at) => {
                    self.debounce_at = None;
                    self.reconcile();
                }
                _ = sleep_opt(self.retry_at) => {
                    self.retry_at = None;
                    self.drive_connection();
                }
                _ = tick.tick() => self.on_tick(),
            }
        }
        self.disconnect();
    }

    fn spawn_task<F>(&self, fut: F)
    where
        F: Future<Output = Option<Event>> + Send + 'static,
    {
        let events = self.events.clone();
        tokio::spawn(async move {
            if let Some(ev) = fut.await {
                let _ = events.send(ev);
            }
        });
    }

    fn saxo_connected(&self) -> bool {
        matches!(&self.conn, Conn::Live(live) if live.resynced)
    }

    fn update_connected_gauge(&self) {
        gauge!("saxo_quote_stream_connected").set(if self.saxo_connected() { 1.0 } else { 0.0 });
    }

    fn rest_token(&self) -> Option<String> {
        current_token(&self.deps.token)
            .or_else(|| match &self.conn {
                Conn::Live(live) => Some(live.token.clone()),
                _ => None,
            })
            .filter(|t| match &self.rejected_token {
                Some((rejected, at)) => rejected != t || at.elapsed() >= self.settings.backoff_max,
                None => true,
            })
    }

    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Join { id, tx, close } => {
                self.clients.insert(
                    id,
                    Client {
                        tx,
                        close: Some(close),
                        watch: HashSet::new(),
                        pending: HashMap::new(),
                        errors: VecDeque::new(),
                        full_since: None,
                    },
                );
                gauge!("saxo_quote_stream_clients").set(self.clients.len() as f64);
                self.send_heartbeat(id);
            }
            Command::Watch { id, pairs } => self.on_watch(id, pairs),
            Command::Leave { id, reason } => self.remove_client(id, reason),
            Command::Invalid { id, reason } => {
                let text = error_text(None, &reason);
                self.deliver_error(id, text);
            }
        }
    }

    fn on_watch(&mut self, id: u64, pairs: Vec<Pair>) {
        let Some(client) = self.clients.get(&id) else {
            return;
        };
        let mut invalid = Vec::new();
        let mut list = HashSet::new();
        for (t, m) in pairs {
            if t.trim().is_empty() || m.trim().is_empty() {
                invalid.push((t, m));
            } else {
                list.insert(saxo_quotes::key(&t, &m));
            }
        }
        let added: Vec<Pair> = list.difference(&client.watch).cloned().collect();
        if let Some(client) = self.clients.get_mut(&id) {
            client.watch = list;
            client.pending.retain(|p, _| client.watch.contains(p));
        }
        for p in &invalid {
            let text = error_text(Some(p), "ticker and mic are required");
            self.deliver_error(id, text);
        }
        self.recompute_union();
        for pair in &added {
            let Some(&uic) = self.resolved.get(pair) else {
                continue;
            };
            if self.refused.contains(&uic) {
                let text = error_text(Some(pair), "Saxo refused the subscription");
                self.deliver_error(id, text);
                continue;
            }
            if let Some(entry) = self.quotes.get(&uic) {
                let seq = self.seqs.get(&uic).copied().unwrap_or(0);
                let text = quote_text(pair, entry, seq);
                self.deliver(
                    id,
                    Some(pair),
                    Outgoing {
                        text,
                        received: None,
                    },
                );
            }
        }
        self.drive_connection();
    }

    fn remove_client(&mut self, id: u64, reason: &'static str) {
        let Some(mut client) = self.clients.remove(&id) else {
            return;
        };
        if let Some(close) = client.close.take() {
            let _ = close.send(reason);
        }
        counter!("saxo_quote_stream_client_closed_total", "reason" => reason).increment(1);
        gauge!("saxo_quote_stream_clients").set(self.clients.len() as f64);
        info!(client = id, reason, "quote stream client closed");
        self.recompute_union();
    }

    fn deliver_error(&mut self, id: u64, text: Utf8Bytes) {
        let alive = match self.clients.get_mut(&id) {
            Some(c) => c.deliver_error(Outgoing {
                text,
                received: None,
            }),
            None => return,
        };
        if !alive {
            self.remove_client(id, REASON_CLIENT);
        }
    }

    fn deliver(&mut self, id: u64, pair: Option<&Pair>, out: Outgoing) {
        let alive = match self.clients.get_mut(&id) {
            Some(c) => c.deliver(pair, out),
            None => return,
        };
        if !alive {
            self.remove_client(id, REASON_CLIENT);
        }
    }

    fn recompute_union(&mut self) {
        let union: HashSet<Pair> = self
            .clients
            .values()
            .flat_map(|c| c.watch.iter().cloned())
            .collect();
        if union != self.union {
            let now = Instant::now();
            for gone in self.union.difference(&union) {
                if let Some(uic) = self.resolved.get(gone) {
                    self.refused.remove(uic);
                    self.solo.remove(uic);
                }
            }
            self.union = union;
            self.debounce_at.get_or_insert(now + self.settings.debounce);
            gauge!("saxo_quote_stream_watched_instruments").set(self.union.len() as f64);
        }
        if self.union.is_empty() {
            self.empty_since.get_or_insert_with(Instant::now);
        } else {
            self.empty_since = None;
        }
        if self.refresh_resolution() {
            self.debounce_at
                .get_or_insert(Instant::now() + self.settings.debounce);
        }
    }

    /// Reads the union's Uics from the shared map. `true` when the set of
    /// resolved pairs changed.
    fn refresh_resolution(&mut self) -> bool {
        let now = Instant::now();
        let mut resolved = HashMap::new();
        {
            let map = self.deps.uics.read().unwrap_or_else(|e| e.into_inner());
            for pair in &self.union {
                if let Some(inst) = map.get(pair) {
                    resolved.insert(pair.clone(), inst.uic);
                }
            }
        }
        self.unresolved
            .retain(|p, _| self.union.contains(p) && !resolved.contains_key(p));
        for pair in &self.union {
            if resolved.contains_key(pair) || self.unresolved.contains_key(pair) {
                continue;
            }
            if self.deps.unknown.try_send(pair.clone()).is_err() {
                debug!(ticker = %pair.0, mic = %pair.1, "background resolve queue full");
            }
            self.unresolved.insert(
                pair.clone(),
                Unresolved {
                    since: now,
                    queued_at: now,
                    notified: HashSet::new(),
                },
            );
        }
        let changed = resolved != self.resolved;
        if changed {
            let mut by_uic: HashMap<u64, Vec<Pair>> = HashMap::new();
            for (pair, uic) in &resolved {
                by_uic.entry(*uic).or_default().push(pair.clone());
            }
            self.by_uic = by_uic;
            self.resolved = resolved;
        }
        changed
    }

    fn desired_uics(&self) -> HashSet<u64> {
        self.resolved
            .values()
            .copied()
            .filter(|u| !self.refused.contains(u))
            .collect()
    }

    fn new_reference(&mut self) -> String {
        let r = format!("q{}", self.next_ref);
        self.next_ref += 1;
        r
    }

    fn spawn_subscribe(&self, ctx: &str, reference: &str, uics: Vec<u64>, replace: Option<String>) {
        let (ctx, reference) = (ctx.to_string(), reference.to_string());
        let Some(token) = self.rest_token() else {
            let _ = self.events.send(Event::Subscribed {
                ctx,
                reference,
                replace,
                token: String::new(),
                result: Err(StreamError::no_token()),
                at: Instant::now(),
                at_utc: Utc::now(),
            });
            return;
        };
        let client = self.deps.client.clone();
        self.spawn_task(async move {
            let result = client
                .subscribe(&token, &ctx, &reference, &uics, replace.as_deref())
                .await;
            Some(Event::Subscribed {
                ctx,
                reference,
                replace,
                token,
                result,
                at: Instant::now(),
                at_utc: Utc::now(),
            })
        });
    }

    fn spawn_unsubscribe(&self, ctx: &str, reference: Option<&str>) {
        let Some(token) = self.rest_token() else {
            return;
        };
        let client = self.deps.client.clone();
        let (ctx, reference) = (ctx.to_string(), reference.map(str::to_string));
        self.spawn_task(async move {
            if let Err(e) = client.unsubscribe(&token, &ctx, reference.as_deref()).await {
                debug!(ctx, reference, error = %e, "deleting a Saxo subscription failed");
            }
            None
        });
    }

    /// Brings the Saxo subscriptions in line with the watched Uics.
    fn reconcile(&mut self) {
        let desired = self.desired_uics();
        let max = self.settings.max_uics_per_subscription.max(1);
        let Conn::Live(live) = &mut self.conn else {
            return;
        };
        let ctx = live.ctx.clone();
        let mut deletes = Vec::new();
        let mut replaces = Vec::new();
        let mut covered: HashSet<u64> = HashSet::new();
        let mut defer = false;
        for (reference, sub) in &live.subs {
            let keep: Vec<u64> = sub
                .uics
                .iter()
                .copied()
                .filter(|u| desired.contains(u))
                .collect();
            if keep.len() == sub.uics.len() {
                covered.extend(keep);
                continue;
            }
            if !sub.active {
                defer = true;
                covered.extend(sub.uics.iter().copied());
                continue;
            }
            covered.extend(keep.iter().copied());
            if keep.is_empty() {
                deletes.push(reference.clone());
            } else {
                replaces.push((reference.clone(), keep));
            }
        }
        self.reconcile_after_pending = defer;
        let now = Instant::now();
        self.sub_not_before
            .retain(|u, until| desired.contains(u) && *until > now);
        let mut wake: Option<Instant> = None;
        let mut fresh: Vec<u64> = Vec::new();
        for uic in desired.difference(&covered) {
            match self.sub_not_before.get(uic) {
                Some(until) => wake = Some(wake.map_or(*until, |w| w.min(*until))),
                None => fresh.push(*uic),
            }
        }
        fresh.sort_unstable();
        let (solo, batched): (Vec<u64>, Vec<u64>) =
            fresh.into_iter().partition(|u| self.solo.contains(u));
        let mut chunks: Vec<Vec<u64>> = batched.chunks(max).map(<[u64]>::to_vec).collect();
        chunks.extend(solo.into_iter().map(|u| vec![u]));

        let mut dropped: Vec<u64> = Vec::new();
        let mut creates: Vec<(String, Vec<u64>, Option<String>)> = Vec::new();
        for reference in &deletes {
            if let Some(sub) = live.subs.remove(reference) {
                dropped.extend(sub.uics);
            }
            live.awaiting_snapshot.remove(reference);
        }
        for (old, keep) in replaces {
            if let Some(sub) = live.subs.remove(&old) {
                dropped.extend(sub.uics.into_iter().filter(|u| !keep.contains(u)));
            }
            live.awaiting_snapshot.remove(&old);
            creates.push((String::new(), keep, Some(old)));
        }
        for chunk in chunks {
            creates.push((String::new(), chunk, None));
        }
        let ref_start = self.next_ref;
        self.next_ref += creates.len() as u64;
        let Conn::Live(live) = &mut self.conn else {
            return;
        };
        for (i, c) in creates.iter_mut().enumerate() {
            c.0 = format!("q{}", ref_start + i as u64);
            live.subs.insert(
                c.0.clone(),
                Sub {
                    uics: c.1.clone(),
                    active: false,
                    inactivity: None,
                    buffered: Vec::new(),
                },
            );
            if !live.resynced {
                live.awaiting_snapshot.insert(c.0.clone());
            }
        }
        if !live.resynced && live.awaiting_snapshot.is_empty() {
            live.resynced = true;
        }
        for uic in dropped {
            if !desired.contains(&uic) {
                self.quotes.remove(&uic);
            }
        }
        for reference in deletes {
            info!(ctx, reference, "deleting Saxo subscription nobody watches");
            self.spawn_unsubscribe(&ctx, Some(&reference));
        }
        for (reference, uics, replace) in creates {
            info!(ctx, reference, uics = ?uics, replace, "subscribing Saxo info prices");
            self.spawn_subscribe(&ctx, &reference, uics, replace);
        }
        if let Some(w) = wake {
            self.debounce_at = Some(self.debounce_at.map_or(w, |d| d.min(w)));
        }
        self.update_connected_gauge();
    }

    /// Holds `uics` back from subscribing for an exponentially growing wait
    /// (`backoff_min` doubling to `backoff_max`), at least Saxo's
    /// `Retry-After` when it sent one.
    fn back_off_uics(&mut self, uics: &[u64], retry_after: Option<Duration>) {
        let now = Instant::now();
        let mut wake = now;
        for uic in uics {
            let n = self.sub_failures.entry(*uic).or_insert(0);
            *n = n.saturating_add(1);
            let doubled = self
                .settings
                .backoff_min
                .saturating_mul(1u32 << (*n - 1).min(16));
            let mut wait = doubled.min(self.settings.backoff_max);
            if let Some(r) = retry_after {
                wait = wait.max(r.min(MAX_RETRY_AFTER));
            }
            let until = now + wait;
            self.sub_not_before.insert(*uic, until);
            wake = wake.max(until);
        }
        let earliest = uics
            .iter()
            .filter_map(|u| self.sub_not_before.get(u).copied())
            .min()
            .unwrap_or(wake);
        self.debounce_at = Some(self.debounce_at.map_or(earliest, |d| d.min(earliest)));
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Connected { ctx, token, result } => self.on_connected(ctx, token, result),
            Event::Subscribed {
                ctx,
                reference,
                replace,
                token,
                result,
                at,
                at_utc,
            } => self.on_subscribed(ctx, reference, replace, token, result, at, at_utc),
            Event::Reauthorized { ctx, token, ok } => {
                let (min, max) = (self.settings.backoff_min, self.settings.backoff_max);
                if let Conn::Live(live) = &mut self.conn {
                    if live.ctx == ctx {
                        live.reauth_inflight = false;
                        if ok {
                            live.token = token;
                            live.reauth_failures = 0;
                            live.reauth_next = None;
                            info!(ctx, "Saxo streaming connection re-authorized");
                        } else {
                            live.reauth_failures = live.reauth_failures.saturating_add(1);
                            let wait = min
                                .saturating_mul(1u32 << (live.reauth_failures - 1).min(16))
                                .min(max);
                            live.reauth_next = Some(Instant::now() + wait);
                            counter!("saxo_quote_stream_reauthorize_failures_total").increment(1);
                        }
                    }
                }
            }
        }
    }

    fn on_connected(
        &mut self,
        ctx: String,
        token: String,
        result: Result<Box<SaxoSocket>, StreamError>,
    ) {
        let Conn::Connecting { ctx: want } = &self.conn else {
            return;
        };
        if *want != ctx {
            return;
        }
        match result {
            Ok(socket) => {
                info!(ctx, "Saxo streaming connected");
                self.conn = Conn::Live(Box::new(Live {
                    ctx,
                    socket: *socket,
                    token,
                    last_msg: Instant::now(),
                    got_message: false,
                    subs: HashMap::new(),
                    reauth_inflight: false,
                    reauth_failures: 0,
                    reauth_next: None,
                    awaiting_snapshot: HashSet::new(),
                    resynced: false,
                }));
                self.refused.clear();
                self.sub_failures.clear();
                self.sub_not_before.clear();
                self.reconcile();
            }
            Err(e) => {
                warn!(ctx, error = %e, "Saxo streaming connect failed");
                self.enter_backoff();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_subscribed(
        &mut self,
        ctx: String,
        reference: String,
        replace: Option<String>,
        token: String,
        result: Result<Subscribed, StreamError>,
        at: Instant,
        at_utc: DateTime<Utc>,
    ) {
        let Conn::Live(live) = &mut self.conn else {
            return;
        };
        if live.ctx != ctx {
            return;
        }
        let Some(sub) = live.subs.get_mut(&reference) else {
            if result.is_ok() {
                self.spawn_unsubscribe(&ctx, Some(&reference));
            }
            return;
        };
        match result {
            Ok(subscribed) => {
                sub.active = true;
                sub.inactivity = subscribed.inactivity_timeout_secs;
                let uics = sub.uics.clone();
                for uic in &uics {
                    self.sub_failures.remove(uic);
                    self.sub_not_before.remove(uic);
                }
                let buffered = std::mem::take(&mut sub.buffered);
                live.awaiting_snapshot.remove(&reference);
                if !live.resynced && live.awaiting_snapshot.is_empty() {
                    live.resynced = true;
                }
                info!(
                    ctx,
                    reference,
                    instruments = subscribed.snapshot.len(),
                    "Saxo subscription active"
                );
                self.apply(&uics, subscribed.snapshot, at, at_utc, true);
                for (payload, received, received_utc) in buffered {
                    match saxo::parse_deltas(&payload) {
                        Ok(deltas) => self.apply(&uics, deltas, received, received_utc, false),
                        Err(e) => warn!(reference, error = %e, "undecodable Saxo delta"),
                    }
                }
                self.update_connected_gauge();
                if self.reconcile_after_pending {
                    self.reconcile();
                }
            }
            Err(e) => {
                let uics = sub.uics.clone();
                live.subs.remove(&reference);
                live.awaiting_snapshot.remove(&reference);
                if !live.resynced && live.awaiting_snapshot.is_empty() {
                    live.resynced = true;
                }
                if let Some(old) = replace {
                    if e.is_auth() {
                        self.orphans.push((ctx.clone(), old));
                    } else {
                        self.spawn_unsubscribe(&ctx, Some(&old));
                    }
                }
                if e.is_auth() {
                    warn!(ctx, reference, error = %e, "Saxo rejected the access token, waiting for a fresh one or a retry");
                    if !token.is_empty() {
                        self.rejected_token = Some((token, Instant::now()));
                    }
                    self.back_off_uics(&uics, e.retry_after());
                } else if e.is_transient() {
                    warn!(ctx, reference, error = %e, "Saxo subscription failed, backing off");
                    self.back_off_uics(&uics, e.retry_after());
                } else if uics.len() > 1 {
                    warn!(ctx, reference, error = %e, "Saxo refused a batch subscription, subscribing one by one");
                    self.solo.extend(uics);
                    self.debounce_at
                        .get_or_insert(Instant::now() + self.settings.debounce);
                } else {
                    error!(ctx, reference, uics = ?uics, error = %e, "Saxo refused the subscription");
                    let reason = format!("Saxo refused the subscription: {e}");
                    for uic in uics {
                        self.refused.insert(uic);
                        self.quotes.remove(&uic);
                        self.notify_pairs(uic, &reason);
                    }
                }
                self.update_connected_gauge();
            }
        }
    }

    fn notify_pairs(&mut self, uic: u64, reason: &str) {
        let pairs = self.by_uic.get(&uic).cloned().unwrap_or_default();
        for pair in pairs {
            let text = error_text(Some(&pair), reason);
            let ids: Vec<u64> = self
                .clients
                .iter()
                .filter(|(_, c)| c.watch.contains(&pair))
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                self.deliver_error(id, text.clone());
            }
        }
    }

    /// Merges updates for the Uics of one subscription: a snapshot replaces
    /// the stored quote, a delta is merged into it.
    fn apply(
        &mut self,
        uics: &[u64],
        deltas: Vec<InfoPriceDelta>,
        received: Instant,
        received_at: DateTime<Utc>,
        snapshot: bool,
    ) {
        for delta in deltas {
            let uic = match (delta.uic, uics) {
                (Some(u), _) => u,
                (None, [only]) => *only,
                (None, _) => continue,
            };
            if !uics.contains(&uic) {
                continue;
            }
            let entry = if snapshot {
                self.quotes.insert(
                    uic,
                    Entry {
                        quote: StreamQuote::from_snapshot(delta),
                        received_at,
                    },
                );
                self.quotes.get_mut(&uic)
            } else {
                let e = self.quotes.entry(uic).or_insert_with(|| Entry {
                    quote: StreamQuote::default(),
                    received_at,
                });
                e.quote.apply(delta);
                e.received_at = received_at;
                Some(e)
            };
            if let Some(e) = entry {
                record_lag(&e.quote, received_at);
            }
            *self.seqs.entry(uic).or_insert(0) += 1;
            counter!("saxo_quote_stream_updates_total").increment(1);
            self.publish(uic, received);
        }
    }

    fn publish(&mut self, uic: u64, received: Instant) {
        let Some(entry) = self.quotes.get(&uic) else {
            return;
        };
        let seq = self.seqs.get(&uic).copied().unwrap_or(0);
        let Some(pairs) = self.by_uic.get(&uic) else {
            return;
        };
        let mut gone = Vec::new();
        for pair in pairs {
            let text = quote_text(pair, entry, seq);
            for (id, client) in self.clients.iter_mut() {
                if !client.watch.contains(pair) {
                    continue;
                }
                let out = Outgoing {
                    text: text.clone(),
                    received: Some(received),
                };
                if !client.deliver(Some(pair), out) {
                    gone.push(*id);
                }
            }
        }
        for id in gone {
            self.remove_client(id, REASON_CLIENT);
        }
    }

    fn on_frame(
        &mut self,
        frame: Option<Result<SaxoMessage, tokio_tungstenite::tungstenite::Error>>,
    ) {
        let received = Instant::now();
        let received_at = Utc::now();
        let data = match frame {
            Some(Ok(SaxoMessage::Binary(data))) => data,
            Some(Ok(SaxoMessage::Close(frame))) => {
                self.lose_connection(&format!("closed by Saxo: {frame:?}"));
                return;
            }
            Some(Ok(_)) => {
                self.touch(received);
                return;
            }
            Some(Err(e)) => {
                self.lose_connection(&format!("socket error: {e}"));
                return;
            }
            None => {
                self.lose_connection("socket ended");
                return;
            }
        };
        self.touch(received);
        let messages = match saxo::decode_frame(&data) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "undecodable Saxo frame");
                return;
            }
        };
        for msg in messages {
            if let Some(control) = saxo::parse_control(msg.reference_id, msg.payload) {
                self.on_control(control);
                if !matches!(self.conn, Conn::Live(_)) {
                    return;
                }
                continue;
            }
            if msg.format != 0 {
                continue;
            }
            let Conn::Live(live) = &mut self.conn else {
                return;
            };
            let Some(sub) = live.subs.get_mut(msg.reference_id) else {
                continue;
            };
            if !sub.active {
                sub.buffered
                    .push((msg.payload.to_vec(), received, received_at));
                continue;
            }
            let uics = sub.uics.clone();
            match saxo::parse_deltas(msg.payload) {
                Ok(deltas) => self.apply(&uics, deltas, received, received_at, false),
                Err(e) => warn!(reference = msg.reference_id, error = %e, "undecodable Saxo delta"),
            }
        }
    }

    fn touch(&mut self, at: Instant) {
        if let Conn::Live(live) = &mut self.conn {
            live.last_msg = at;
            if !live.got_message {
                live.got_message = true;
                self.backoff = self.settings.backoff_min;
            }
        }
    }

    fn on_control(&mut self, control: Control) {
        match control {
            Control::Heartbeat(beats) => {
                let disabled: Vec<String> = beats
                    .into_iter()
                    .filter(|b| b.reason == "SubscriptionPermanentlyDisabled")
                    .map(|b| b.originating_reference_id)
                    .collect();
                if !disabled.is_empty() {
                    self.on_disabled(disabled);
                }
            }
            Control::Disconnect => self.lose_connection("Saxo sent _disconnect"),
            Control::ResetSubscriptions(targets) => {
                info!(targets = ?targets, "Saxo asked to reset subscriptions");
                self.resubscribe(targets);
            }
            Control::Other(id) => debug!(id, "unknown Saxo control message"),
        }
    }

    /// Saxo says these subscriptions will never get data (for example no
    /// market-data entitlement). A subscription of one instrument is a refusal
    /// until the next reconnect; a batch is split so the offender is found.
    fn on_disabled(&mut self, references: Vec<String>) {
        let Conn::Live(live) = &mut self.conn else {
            return;
        };
        let ctx = live.ctx.clone();
        let mut removed = Vec::new();
        for reference in references {
            if let Some(sub) = live.subs.remove(&reference) {
                live.awaiting_snapshot.remove(&reference);
                removed.push((reference, sub.uics));
            }
        }
        if !live.resynced && live.awaiting_snapshot.is_empty() {
            live.resynced = true;
        }
        for (reference, uics) in removed {
            warn!(ctx, reference, uics = ?uics, "Saxo permanently disabled a subscription");
            self.spawn_unsubscribe(&ctx, Some(&reference));
            if uics.len() > 1 {
                self.solo.extend(uics);
            } else {
                for uic in uics {
                    self.refused.insert(uic);
                    self.quotes.remove(&uic);
                    self.notify_pairs(
                        uic,
                        "Saxo refused the subscription: SubscriptionPermanentlyDisabled",
                    );
                }
            }
        }
        self.debounce_at
            .get_or_insert(Instant::now() + self.settings.debounce);
        self.update_connected_gauge();
    }

    /// Re-creates `targets` (every subscription when empty) under new
    /// reference ids, each replacing the old one.
    fn resubscribe(&mut self, targets: Vec<String>) {
        let Conn::Live(live) = &self.conn else {
            return;
        };
        let mut refs: Vec<String> = if targets.is_empty() {
            live.subs.keys().cloned().collect()
        } else {
            targets
                .into_iter()
                .filter(|t| live.subs.contains_key(t))
                .collect()
        };
        refs.sort();
        for old in refs {
            let new = self.new_reference();
            let Conn::Live(live) = &mut self.conn else {
                return;
            };
            let Some(sub) = live.subs.remove(&old) else {
                continue;
            };
            let uics = sub.uics;
            live.subs.insert(
                new.clone(),
                Sub {
                    uics: uics.clone(),
                    active: false,
                    inactivity: None,
                    buffered: Vec::new(),
                },
            );
            let ctx = live.ctx.clone();
            self.spawn_subscribe(&ctx, &new, uics, Some(old));
        }
    }

    fn enter_backoff(&mut self) {
        if self.union.is_empty() {
            self.conn = Conn::Idle;
            self.update_connected_gauge();
            return;
        }
        let until = Instant::now() + self.backoff;
        info!(wait = ?self.backoff, "Saxo streaming reconnects after backoff");
        self.backoff = (self.backoff * 2).min(self.settings.backoff_max);
        self.conn = Conn::Backoff { until };
        self.retry_at = Some(until);
        self.update_connected_gauge();
    }

    fn close_socket(&mut self) -> Option<String> {
        let conn = std::mem::replace(&mut self.conn, Conn::Idle);
        let Conn::Live(live) = conn else {
            return None;
        };
        let Live {
            ctx, mut socket, ..
        } = *live;
        tokio::spawn(async move {
            let _ = socket.close(None).await;
        });
        Some(ctx)
    }

    fn lose_connection(&mut self, reason: &str) {
        if let Some(ctx) = self.close_socket() {
            warn!(ctx, reason, "Saxo streaming connection lost");
            self.spawn_unsubscribe(&ctx, None);
            counter!("saxo_quote_stream_reconnects_total").increment(1);
        }
        self.enter_backoff();
    }

    fn disconnect(&mut self) {
        if let Some(ctx) = self.close_socket() {
            info!(ctx, "Saxo streaming disconnected, nobody watches");
            self.spawn_unsubscribe(&ctx, None);
        }
        self.conn = Conn::Idle;
        self.retry_at = None;
        self.backoff = self.settings.backoff_min;
        self.update_connected_gauge();
    }

    fn drive_connection(&mut self) {
        if self.union.is_empty() {
            return;
        }
        match &self.conn {
            Conn::Live(_) | Conn::Connecting { .. } => return,
            Conn::Backoff { until } if *until > Instant::now() => {
                self.retry_at = Some(*until);
                return;
            }
            Conn::Backoff { .. } | Conn::Idle => {}
        }
        let Some(token) = current_token(&self.deps.token) else {
            return;
        };
        let ctx = saxo::new_context_id();
        self.conn = Conn::Connecting { ctx: ctx.clone() };
        let client = self.deps.client.clone();
        info!(ctx, "Saxo streaming connecting");
        self.spawn_task(async move {
            let result = client.connect(&ctx, &token).await.map(Box::new);
            Some(Event::Connected { ctx, token, result })
        });
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        let mut dead = false;
        let mut reauth: Option<(String, String)> = None;
        let token = current_token(&self.deps.token);
        if let Conn::Live(live) = &mut self.conn {
            let mut inactivity = None;
            for sub in live.subs.values().filter(|s| s.active) {
                inactivity = Some(inactivity.unwrap_or(0).max(sub.inactivity.unwrap_or(0)));
            }
            match inactivity {
                None => live.last_msg = now,
                Some(secs) => {
                    let allowed = Duration::from_secs(secs) + self.settings.heartbeat_timeout;
                    dead = now.duration_since(live.last_msg) > allowed;
                }
            }
            if !dead {
                if let Some(t) = token {
                    let due = live.reauth_next.is_none_or(|n| now >= n);
                    if !live.reauth_inflight && due && t != live.token {
                        live.reauth_inflight = true;
                        reauth = Some((live.ctx.clone(), t));
                    }
                }
            }
        }
        if !self.orphans.is_empty() {
            let orphans = std::mem::take(&mut self.orphans);
            if let (Conn::Live(live), Some(_)) = (&self.conn, self.rest_token()) {
                let ctx = live.ctx.clone();
                for (c, reference) in orphans.into_iter().filter(|(c, _)| *c == ctx) {
                    self.spawn_unsubscribe(&c, Some(&reference));
                }
            } else if matches!(self.conn, Conn::Live(_)) {
                self.orphans = orphans;
            }
        }
        if dead {
            self.lose_connection("no Saxo message within the heartbeat timeout");
        }
        if let Some((ctx, token)) = reauth {
            let client = self.deps.client.clone();
            self.spawn_task(async move {
                let ok = match client.reauthorize(&token, &ctx).await {
                    Ok(()) => true,
                    Err(e) => {
                        warn!(ctx, error = %e, "Saxo streaming re-authorization failed");
                        false
                    }
                };
                Some(Event::Reauthorized { ctx, token, ok })
            });
        }
        if self
            .empty_since
            .is_some_and(|t| now.duration_since(t) >= self.settings.idle_disconnect)
            && !matches!(self.conn, Conn::Idle)
        {
            self.disconnect();
        }

        if self.refresh_resolution() {
            self.debounce_at.get_or_insert(now + self.settings.debounce);
        }
        let mut errors: Vec<(Pair, Vec<u64>)> = Vec::new();
        for (pair, u) in self.unresolved.iter_mut() {
            if now.duration_since(u.queued_at) >= self.settings.resolve_retry {
                u.queued_at = now;
                let _ = self.deps.unknown.try_send(pair.clone());
            }
            if now.duration_since(u.since) >= self.settings.unresolved_error_after {
                let ids: Vec<u64> = self
                    .clients
                    .iter()
                    .filter(|(id, c)| c.watch.contains(pair) && !u.notified.contains(*id))
                    .map(|(id, _)| *id)
                    .collect();
                u.notified.extend(ids.iter().copied());
                if !ids.is_empty() {
                    errors.push((pair.clone(), ids));
                }
            }
        }
        for (pair, ids) in errors {
            let text = error_text(
                Some(&pair),
                "not a registered company with a resolved Saxo Uic",
            );
            for id in ids {
                self.deliver_error(id, text.clone());
            }
        }

        let mut slow = Vec::new();
        let mut gone = Vec::new();
        for (id, client) in self.clients.iter_mut() {
            if !client.flush() {
                gone.push(*id);
            } else if client
                .full_since
                .is_some_and(|t| now.duration_since(t) >= self.settings.slow_client_grace)
            {
                slow.push(*id);
            }
        }
        for id in gone {
            self.remove_client(id, REASON_CLIENT);
        }
        for id in slow {
            warn!(client = id, "quote stream client too slow, closing it");
            self.remove_client(id, REASON_SLOW);
        }
        let ids: Vec<u64> = self.clients.keys().copied().collect();
        for id in ids {
            self.send_heartbeat(id);
        }
        self.drive_connection();
    }

    fn send_heartbeat(&mut self, id: u64) {
        let connected = self.saxo_connected();
        let Some(client) = self.clients.get(&id) else {
            return;
        };
        if !client.pending.is_empty() || !client.errors.is_empty() {
            return;
        }
        let mut unresolved: Vec<PairBody<'_>> = client
            .watch
            .iter()
            .filter(|p| self.unresolved.contains_key(*p))
            .map(|(t, m)| PairBody { ticker: t, mic: m })
            .collect();
        unresolved.sort_by(|a, b| (a.ticker, a.mic).cmp(&(b.ticker, b.mic)));
        let text = to_text(&HeartbeatMsg {
            kind: "heartbeat",
            at: rfc3339(Utc::now()),
            saxo_connected: connected,
            unresolved,
        });
        self.deliver(
            id,
            None,
            Outgoing {
                text,
                received: None,
            },
        );
    }
}

/// One watched pair as a client sends it.
#[derive(Debug, Deserialize)]
struct WatchPair {
    ticker: String,
    mic: String,
}

/// A client's message: its whole new watch list.
#[derive(Debug, Deserialize)]
struct WatchMsg {
    watch: Vec<WatchPair>,
}

/// The router serving `GET /quotes/stream`.
///
/// Client → server: `{"watch":[{"ticker":"...","mic":"..."},...]}`, each
/// message replacing that client's watch list.
///
/// Server → client, one JSON object per message:
/// - `{"type":"quote","ticker","mic","bid","ask","bid_size","ask_size","bid_type","ask_type","market_state","delayed_minutes","currency","saxo_time","received_at","seq"}`,
///   prices as decimal strings, `null` for what Saxo has not sent, `seq`
///   increasing per instrument; sent for every update and, once, for each
///   newly watched pair that already has a quote;
/// - `{"type":"heartbeat","at","saxo_connected","unresolved":[{"ticker","mic"}]}`
///   every second; while `saxo_connected` is false prices are unconfirmed;
/// - `{"type":"error","pair":{"ticker","mic"}|null,"reason"}` for a pair that
///   cannot be resolved or subscribed, or a message that is not a watch list.
pub fn router(handle: QuoteStreamHandle) -> Router {
    Router::new()
        .route("/quotes/stream", get(upgrade))
        .with_state(handle)
}

async fn upgrade(ws: WebSocketUpgrade, State(handle): State<QuoteStreamHandle>) -> Response {
    ws.on_upgrade(move |socket| serve_client(socket, handle))
}

async fn serve_client(socket: WebSocket, handle: QuoteStreamHandle) {
    let Some(Subscriber {
        id,
        mut rx,
        mut close,
    }) = handle.join().await
    else {
        return;
    };
    info!(client = id, "quote stream client connected");
    let (mut sink, mut stream) = socket.split();
    let writer_handle = handle.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                reason = &mut close => {
                    if let Ok(reason) = reason {
                        let frame = CloseFrame {
                            code: axum::extract::ws::close_code::POLICY,
                            reason: Utf8Bytes::from(reason),
                        };
                        let _ = sink.send(Message::Close(Some(frame))).await;
                    }
                    return;
                }
                msg = rx.recv() => {
                    let Some(msg) = msg else { return };
                    if sink.send(Message::Text(msg.text)).await.is_err() {
                        writer_handle.leave(id, REASON_SEND_FAILED).await;
                        return;
                    }
                    if let Some(received) = msg.received {
                        histogram!("saxo_quote_stream_fanout_seconds")
                            .record(received.elapsed().as_secs_f64());
                    }
                }
            }
        }
    });
    let mut writer = writer;
    loop {
        let msg = tokio::select! {
            msg = stream.next() => msg,
            _ = &mut writer => break,
        };
        let Some(Ok(msg)) = msg else { break };
        match msg {
            Message::Text(text) => match serde_json::from_str::<WatchMsg>(text.as_str()) {
                Ok(w) => {
                    let pairs = w.watch.into_iter().map(|p| (p.ticker, p.mic)).collect();
                    handle.watch(id, pairs).await;
                }
                Err(e) => {
                    handle
                        .invalid(
                            id,
                            format!("expected {{\"watch\":[{{\"ticker\",\"mic\"}}]}}: {e}"),
                        )
                        .await;
                }
            },
            Message::Close(_) => break,
            _ => {}
        }
    }
    handle.leave(id, REASON_CLIENT).await;
    writer.abort();
}

#[cfg(test)]
mod tests;
