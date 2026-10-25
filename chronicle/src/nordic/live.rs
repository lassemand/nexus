//! Live listener for MFN's Nordic news stream (NEX-170).
//!
//! # Unofficial endpoint
//!
//! `wss://mfn.se/all/s/nordic.json` is the WebSocket MFN's own website uses; it is
//! undocumented and may change without notice. It carries every Nordic release
//! as one JSON Feed document per message (usually one item), has no replay, and
//! the server closes the connection every 300 s with code 3000 ("fast
//! reconnect"). **Fallback:** the hourly `nordic-reports` poll reads the same
//! items from MFN's feed, so anything missed while disconnected is still
//! ingested, only later. MFN's `robots.txt` does not apply to WebSocket upgrades,
//! and the repository owner approved MFN JSON access anyway (2026-10-21).
//!
//! This first step only **logs**. It writes nothing and publishes nothing; the
//! poll stays the only writer.
//!
//! # Layout
//!
//! * [`process_message`] is pure: message in, relevant items out.
//! * [`dispose`] asks signal's database whether the poll already stored an item
//!   (the poll "cancels" the live path for it).
//! * [`Listener`] owns the connection and its reconnect rules and hands raw
//!   messages to a channel.

use super::classify::classify_item;
use super::feed::{parse_document, NewsItem};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::USER_AGENT;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Default stream: all Nordic news.
pub const DEFAULT_WS_URL: &str = "wss://mfn.se/all/s/nordic.json";
/// Close code the server uses every 300 s: reconnect at once.
pub const FAST_RECONNECT_CODE: u16 = 3000;
/// No message and no ping/pong for this long means a dead connection.
pub const DEAD_AFTER: Duration = Duration::from_secs(15 * 60);
/// A connection that stayed up this long resets the backoff.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);
/// How many recent `news_id`s are remembered to drop duplicates.
pub const SEEN_CAPACITY: usize = 10_000;
/// How long a connection attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

// ── Registered companies ────────────────────────────────────────────────────

/// A company registered with an MFN slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub ticker: String,
    pub exchange_mic: String,
    pub mfn_slug: String,
}

/// Lookup of registered companies by MFN slug.
#[derive(Debug, Clone, Default)]
pub struct Companies {
    by_slug: HashMap<String, Registered>,
}

impl Companies {
    /// Builds the lookup.
    pub fn new(companies: Vec<Registered>) -> Self {
        Self {
            by_slug: companies
                .into_iter()
                .map(|c| (c.mfn_slug.clone(), c))
                .collect(),
        }
    }

    /// Number of registered companies.
    pub fn len(&self) -> usize {
        self.by_slug.len()
    }

    /// True if nobody is registered.
    pub fn is_empty(&self) -> bool {
        self.by_slug.is_empty()
    }

    /// The registered company that authored `item`: `author.slug`, or any entry
    /// of `author.slugs`, equals its `mfn_slug`.
    pub fn find(&self, item: &NewsItem) -> Option<&Registered> {
        self.by_slug.get(&item.author_slug).or_else(|| {
            item.author_slugs
                .iter()
                .find_map(|s| self.by_slug.get(s.as_str()))
        })
    }
}

// ── De-duplication ──────────────────────────────────────────────────────────

/// The last [`SEEN_CAPACITY`] `news_id`s, so an item that arrives twice (for
/// example around a reconnect) is handled once.
#[derive(Debug)]
pub struct SeenSet {
    ids: HashSet<Uuid>,
    order: VecDeque<Uuid>,
    capacity: usize,
}

impl SeenSet {
    /// A set remembering at most `capacity` ids.
    pub fn new(capacity: usize) -> Self {
        Self {
            ids: HashSet::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    /// Remembers `id`; true if it was not remembered before.
    pub fn insert(&mut self, id: Uuid) -> bool {
        if !self.ids.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.ids.remove(&old);
            }
        }
        true
    }
}

// ── Items ───────────────────────────────────────────────────────────────────

/// A relevant item with everything the log line carries.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveItem {
    pub news_id: Uuid,
    pub ticker: String,
    pub exchange_mic: String,
    pub mfn_slug: String,
    pub publish_date: DateTime<Utc>,
    /// When the message was read from the socket.
    pub received_at: DateTime<Utc>,
    pub lang: Option<String>,
    pub tags: Vec<String>,
    pub source: Option<String>,
    /// `interim`, `annual`, or `None` for anything that is not a results release.
    pub report_kind: Option<&'static str>,
    pub is_correction: bool,
    pub calendar_update: bool,
    pub title: String,
    pub url: Option<String>,
    pub attachment_urls: Vec<String>,
}

impl LiveItem {
    /// `received_at − publish_date` in milliseconds. Negative means clock skew
    /// and is kept as is.
    pub fn latency_ms(&self) -> i64 {
        (self.received_at - self.publish_date).num_milliseconds()
    }

    /// `report_released` for results releases, `company_news` for the rest.
    pub fn event(&self) -> &'static str {
        if self.report_kind.is_some() {
            "report_released"
        } else {
            "company_news"
        }
    }

    /// Logs the item once, as one structured event.
    pub fn log(&self) {
        let tags = serde_json::to_string(&self.tags).unwrap_or_default();
        let attachments = serde_json::to_string(&self.attachment_urls).unwrap_or_default();
        info!(
            event = self.event(),
            news_id = %self.news_id,
            ticker = %self.ticker,
            exchange_mic = %self.exchange_mic,
            mfn_slug = %self.mfn_slug,
            publish_date = %self.publish_date.to_rfc3339(),
            received_at = %self.received_at.to_rfc3339(),
            latency_ms = self.latency_ms(),
            lang = self.lang.as_deref().unwrap_or(""),
            tags = %tags,
            source = self.source.as_deref().unwrap_or(""),
            report_kind = self.report_kind,
            is_correction = self.is_correction,
            calendar_update = self.calendar_update,
            title = %self.title,
            url = self.url.as_deref().unwrap_or(""),
            attachment_urls = %attachments,
            "nordic-live item"
        );
    }
}

/// What one message contained.
#[derive(Debug, Default)]
pub struct Processed {
    /// Items in the message (parsed ones).
    pub items_seen: usize,
    /// New relevant items, in message order.
    pub relevant: Vec<LiveItem>,
    /// Relevant items dropped because their `news_id` was seen recently.
    pub duplicates: usize,
    /// Set when the whole message was not a usable JSON Feed document.
    pub malformed: Option<String>,
    /// One note per item that could not be parsed.
    pub bad_items: Vec<String>,
}

/// Parses one WebSocket message and picks out the relevant new items. Pure
/// apart from updating `seen`. Messages with no items are fine.
pub fn process_message(
    msg: &str,
    received_at: DateTime<Utc>,
    companies: &Companies,
    seen: &mut SeenSet,
) -> Processed {
    let mut out = Processed::default();
    let doc = match parse_document(msg) {
        Ok(d) => d,
        Err(e) => {
            out.malformed = Some(e.to_string());
            return out;
        }
    };
    out.bad_items = doc.bad_items;
    for item in doc.items {
        out.items_seen += 1;
        let Some(company) = companies.find(&item) else {
            continue;
        };
        if !seen.insert(item.news_id) {
            out.duplicates += 1;
            continue;
        }
        // The same rules as the hourly poll (NEX-166).
        let c = classify_item(&item.tags, &item.title);
        out.relevant.push(LiveItem {
            news_id: item.news_id,
            ticker: company.ticker.clone(),
            exchange_mic: company.exchange_mic.clone(),
            mfn_slug: company.mfn_slug.clone(),
            publish_date: item.published_at,
            received_at,
            lang: item.lang,
            tags: item.tags,
            source: item.source,
            report_kind: c.report_kind.map(|k| k.as_str()),
            is_correction: c.is_correction,
            calendar_update: c.calendar_update,
            title: item.title,
            url: item.url,
            attachment_urls: item.attachment_urls,
        });
    }
    out
}

// ── The poll's "cancel" ─────────────────────────────────────────────────────

/// Answers whether the poll already stored an item.
#[async_trait]
pub trait IngestedCheck: Send + Sync {
    /// `Ok(true)` if `news_id` is stored. An error means "could not tell".
    async fn already_ingested(&self, news_id: Uuid) -> Result<bool, String>;
}

/// Looks the item up in signal's `special_events`.
pub struct SignalDb(pub PgPool);

#[async_trait]
impl IngestedCheck for SignalDb {
    async fn already_ingested(&self, news_id: Uuid) -> Result<bool, String> {
        let lookup = sqlx::query_scalar::<_, i32>(
            "SELECT 1 FROM special_events WHERE source = 'mfn' AND source_id = $1 LIMIT 1",
        )
        .bind(news_id.to_string())
        .fetch_optional(&self.0);
        match tokio::time::timeout(Duration::from_secs(5), lookup).await {
            Ok(Ok(row)) => Ok(row.is_some()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("lookup timed out".to_string()),
        }
    }
}

/// What to do with a relevant item.
#[derive(Debug, Clone, PartialEq)]
pub enum Disposition {
    /// Not stored yet: log it.
    New(Box<LiveItem>),
    /// The poll got there first.
    AlreadyIngested(Uuid),
}

/// Applies the poll's cancel. Without a checker, or when the lookup fails, the
/// item is treated as new (logged at `warn` on failure): the live path must not
/// depend on the database being up.
pub async fn dispose(item: LiveItem, check: Option<&dyn IngestedCheck>) -> Disposition {
    if let Some(check) = check {
        match check.already_ingested(item.news_id).await {
            Ok(true) => return Disposition::AlreadyIngested(item.news_id),
            Ok(false) => {}
            Err(e) => warn!(news_id = %item.news_id, error = %e,
                            "signal database lookup failed, treating the item as new"),
        }
    }
    Disposition::New(Box::new(item))
}

/// Logs a disposition: the item itself, or `already ingested by poll`.
pub fn report(d: &Disposition) {
    match d {
        Disposition::New(item) => item.log(),
        Disposition::AlreadyIngested(id) => info!(news_id = %id, "already ingested by poll"),
    }
}

/// Counters for the periodic `items seen` line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub seen: usize,
    pub relevant: usize,
}

impl Counters {
    /// The periodic liveness line.
    pub fn log(&self) {
        info!(
            "nordic-live: {} items seen, {} relevant",
            self.seen, self.relevant
        );
    }
}

/// Handles one message end to end: parse, filter, cancel-check, log. Returns
/// what happened, for the counters and for tests.
pub async fn handle_message(
    msg: &str,
    received_at: DateTime<Utc>,
    companies: &Companies,
    seen: &mut SeenSet,
    check: Option<&dyn IngestedCheck>,
) -> (Counters, Vec<Disposition>) {
    let processed = process_message(msg, received_at, companies, seen);
    if let Some(why) = &processed.malformed {
        let head: String = msg.chars().take(300).collect();
        warn!(error = %why, message = %head, "malformed nordic-live message, skipped");
    }
    for why in &processed.bad_items {
        let head: String = msg.chars().take(300).collect();
        warn!(error = %why, message = %head, "unusable item in nordic-live message, skipped");
    }
    let counters = Counters {
        seen: processed.items_seen,
        relevant: processed.relevant.len() + processed.duplicates,
    };
    let mut out = Vec::new();
    for item in processed.relevant {
        let d = dispose(item, check).await;
        report(&d);
        out.push(d);
    }
    (counters, out)
}

// ── Connection ──────────────────────────────────────────────────────────────

/// Reconnect delay: 10 ms, doubling, capped at 30 s (MFN's own site does the same).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    const START: Duration = Duration::from_millis(10);
    const CAP: Duration = Duration::from_secs(30);

    /// A fresh backoff.
    pub fn new() -> Self {
        Self { next: Self::START }
    }

    /// The delay to wait now; the following one is doubled (capped).
    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(Self::CAP);
        d
    }

    /// Back to 10 ms.
    pub fn reset(&mut self) {
        self.next = Self::START;
    }
}

/// A message read from the socket.
#[derive(Debug, Clone)]
pub struct Received {
    pub text: String,
    pub received_at: DateTime<Utc>,
}

/// What happened to the connection; mainly for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnEvent {
    Connected,
    /// Closed with code 3000: reconnecting at once.
    FastReconnect,
    /// Another close or error: waiting `delay` before reconnecting.
    Backoff {
        reason: String,
        delay: Duration,
    },
    /// Nothing heard for [`Listener::dead_after`].
    Dead,
}

enum SessionEnd {
    FastReconnect,
    Failed(String),
    ReceiverGone,
}

/// Keeps one WebSocket connection alive and forwards its text messages.
pub struct Listener {
    pub url: String,
    pub user_agent: String,
    /// See [`DEAD_AFTER`].
    pub dead_after: Duration,
    /// See [`STABLE_AFTER`].
    pub stable_after: Duration,
    /// Connection events, if someone wants them.
    pub events: Option<mpsc::UnboundedSender<ConnEvent>>,
}

impl Listener {
    /// A listener for `url` with the production timings.
    pub fn new(url: impl Into<String>, user_agent: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            user_agent: user_agent.into(),
            dead_after: DEAD_AFTER,
            stable_after: STABLE_AFTER,
            events: None,
        }
    }

    fn emit(&self, e: ConnEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(e);
        }
    }

    /// Runs until `tx` is closed. Never returns because of network trouble.
    pub async fn run(&self, tx: mpsc::Sender<Received>) {
        let mut backoff = Backoff::new();
        loop {
            if tx.is_closed() {
                return;
            }
            let (end, up_for) = self.session(&tx).await;
            match end {
                SessionEnd::ReceiverGone => return,
                SessionEnd::FastReconnect => {
                    debug!("MFN closed the connection with 3000, reconnecting");
                    self.emit(ConnEvent::FastReconnect);
                    if up_for.is_some_and(|d| d >= self.stable_after) {
                        backoff.reset();
                    }
                }
                SessionEnd::Failed(reason) => {
                    if up_for.is_some_and(|d| d >= self.stable_after) {
                        backoff.reset();
                    }
                    let delay = backoff.next_delay();
                    warn!(%reason, ?delay, "MFN connection lost, backing off");
                    self.emit(ConnEvent::Backoff { reason, delay });
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// One connection. Returns how it ended and how long it was up.
    async fn session(&self, tx: &mpsc::Sender<Received>) -> (SessionEnd, Option<Duration>) {
        let mut request = match self.url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => return (SessionEnd::Failed(format!("bad url: {e}")), None),
        };
        if let Ok(ua) = self.user_agent.parse() {
            request.headers_mut().insert(USER_AGENT, ua);
        }
        let connect = tokio_tungstenite::connect_async(request);
        let mut ws = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => return (SessionEnd::Failed(format!("connect failed: {e}")), None),
            Err(_) => return (SessionEnd::Failed("connect timed out".to_string()), None),
        };
        info!(url = %self.url, "connected to MFN");
        self.emit(ConnEvent::Connected);
        let up_since = Instant::now();

        loop {
            // Any frame, including ping and pong, shows the peer is alive.
            let next = match tokio::time::timeout(self.dead_after, ws.next()).await {
                Ok(n) => n,
                Err(_) => {
                    warn!(silent_for = ?self.dead_after, "no message or ping from MFN, dropping the connection");
                    self.emit(ConnEvent::Dead);
                    return (
                        SessionEnd::Failed("no message or ping/pong".to_string()),
                        Some(up_since.elapsed()),
                    );
                }
            };
            match next {
                None => {
                    return (
                        SessionEnd::Failed("stream ended".to_string()),
                        Some(up_since.elapsed()),
                    )
                }
                Some(Err(e)) => {
                    return (SessionEnd::Failed(e.to_string()), Some(up_since.elapsed()))
                }
                Some(Ok(Message::Text(text))) => {
                    let received = Received {
                        text: text.to_string(),
                        received_at: Utc::now(),
                    };
                    if tx.send(received).await.is_err() {
                        return (SessionEnd::ReceiverGone, Some(up_since.elapsed()));
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let up = Some(up_since.elapsed());
                    return match frame {
                        Some(f) if u16::from(f.code) == FAST_RECONNECT_CODE => {
                            (SessionEnd::FastReconnect, up)
                        }
                        other => (
                            SessionEnd::Failed(format!("closed by server: {other:?}")),
                            up,
                        ),
                    };
                }
                // Ping, pong, binary and raw frames: tungstenite answers pings itself.
                Some(Ok(_)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const GOMSPACE: &str = include_str!("../../tests/fixtures/mfn_ws_gomspace_report.json");
    const VOLVO: &str = include_str!("../../tests/fixtures/mfn_ws_volvo_report.json");

    fn companies() -> Companies {
        Companies::new(vec![Registered {
            ticker: "GOMX".into(),
            exchange_mic: "FNSE".into(),
            mfn_slug: "gomspace-group".into(),
        }])
    }

    fn at(secs_after: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 26, 6, 0, 0).unwrap() + chrono::Duration::seconds(secs_after)
    }

    #[test]
    fn a_registered_report_is_a_report_released_event() {
        let mut seen = SeenSet::new(10);
        let p = process_message(GOMSPACE, at(3), &companies(), &mut seen);
        assert_eq!((p.items_seen, p.relevant.len(), p.duplicates), (1, 1, 0));
        let i = &p.relevant[0];
        assert_eq!(i.event(), "report_released");
        assert_eq!(i.report_kind, Some("interim"));
        assert_eq!(
            (
                i.ticker.as_str(),
                i.exchange_mic.as_str(),
                i.mfn_slug.as_str()
            ),
            ("GOMX", "FNSE", "gomspace-group")
        );
        assert_eq!(i.publish_date, at(0));
        assert_eq!(i.received_at, at(3));
        assert_eq!(i.latency_ms(), 3000);
        assert!(!i.is_correction && !i.calendar_update);
        assert_eq!(i.lang.as_deref(), Some("en"));
        assert_eq!(i.source.as_deref(), Some("cis"));
        assert!(i.tags.contains(&"sub:report:interim".to_string()));
        assert!(i.title.starts_with("Q2 Financial Reporting"));
        assert!(i.url.as_deref().unwrap().starts_with("https://mfn.se/"));
        // Only the PDFs, not the image.
        assert_eq!(i.attachment_urls.len(), 2);
        assert!(i.attachment_urls.iter().all(|u| u.ends_with(".pdf")));
    }

    #[test]
    fn negative_latency_is_kept() {
        let mut seen = SeenSet::new(10);
        let p = process_message(GOMSPACE, at(-5), &companies(), &mut seen);
        assert_eq!(p.relevant[0].latency_ms(), -5000);
    }

    #[test]
    fn other_companies_are_counted_but_not_relevant() {
        let mut seen = SeenSet::new(10);
        let p = process_message(VOLVO, at(0), &companies(), &mut seen);
        assert_eq!((p.items_seen, p.relevant.len(), p.duplicates), (1, 0, 0));
    }

    #[test]
    fn author_slugs_also_count() {
        let both = Companies::new(vec![Registered {
            ticker: "VOLV B".into(),
            exchange_mic: "XSTO".into(),
            mfn_slug: "volvo-group".into(),
        }]);
        // `author.slug` is "volvo"; the registered slug is only among `author.slugs`.
        let mut doc: serde_json::Value = serde_json::from_str(VOLVO).unwrap();
        doc["items"][0]["author"]["slugs"] = serde_json::json!(["volvo", "volvo-group"]);
        doc["items"][0]["author"]["slug"] = "volvo".into();
        let p = process_message(&doc.to_string(), at(0), &both, &mut SeenSet::new(10));
        assert_eq!(p.relevant.len(), 1);
        assert_eq!(p.relevant[0].mfn_slug, "volvo-group");
    }

    #[test]
    fn an_empty_items_array_and_a_missing_one_are_fine() {
        for msg in [
            r#"{"version":"x","items":[]}"#,
            r#"{"version":"x"}"#,
            r#"{"items":null}"#,
        ] {
            let p = process_message(msg, at(0), &companies(), &mut SeenSet::new(10));
            assert_eq!((p.items_seen, p.relevant.len()), (0, 0));
            assert!(p.malformed.is_none(), "{msg}");
        }
    }

    #[test]
    fn malformed_messages_and_items_are_reported_not_fatal() {
        for msg in ["not json at all", "[1,2,3]", r#"{"items": "oops"}"#, ""] {
            let p = process_message(msg, at(0), &companies(), &mut SeenSet::new(10));
            assert!(p.malformed.is_some(), "{msg:?}");
            assert!(p.relevant.is_empty());
        }
        // One good and two bad items in a single message: the good one survives.
        let good: serde_json::Value = serde_json::from_str(GOMSPACE).unwrap();
        let item = good["items"][0].clone();
        let mut no_id = item.clone();
        no_id.as_object_mut().unwrap().remove("news_id");
        let mut bad_date = item.clone();
        bad_date["content"]["publish_date"] = "yesterday".into();
        let msg = serde_json::json!({"items": [no_id, bad_date, item]}).to_string();
        let p = process_message(&msg, at(0), &companies(), &mut SeenSet::new(10));
        assert_eq!(p.relevant.len(), 1);
        assert_eq!(p.bad_items.len(), 2);
    }

    #[test]
    fn the_same_news_id_is_logged_once() {
        let mut seen = SeenSet::new(10);
        let first = process_message(GOMSPACE, at(1), &companies(), &mut seen);
        let again = process_message(GOMSPACE, at(2), &companies(), &mut seen);
        assert_eq!(
            (first.relevant.len(), again.relevant.len(), again.duplicates),
            (1, 0, 1)
        );
    }

    #[test]
    fn the_seen_set_forgets_the_oldest_beyond_its_capacity() {
        let mut s = SeenSet::new(3);
        let ids: Vec<Uuid> = (1..=4).map(Uuid::from_u128).collect();
        for id in &ids {
            assert!(s.insert(*id));
        }
        assert!(s.insert(ids[0]), "the oldest was forgotten");
        assert!(!s.insert(ids[3]), "the newest is still remembered");
    }

    #[test]
    fn backoff_doubles_from_10ms_to_30s_and_resets() {
        let mut b = Backoff::new();
        let delays: Vec<Duration> = (0..14).map(|_| b.next_delay()).collect();
        assert_eq!(delays[0], Duration::from_millis(10));
        assert_eq!(delays[1], Duration::from_millis(20));
        assert_eq!(delays[2], Duration::from_millis(40));
        assert_eq!(delays[11], Duration::from_millis(20_480));
        assert_eq!(delays[12], Duration::from_secs(30));
        assert_eq!(delays[13], Duration::from_secs(30));
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(10));
    }

    struct Stub(Result<bool, String>);

    #[async_trait]
    impl IngestedCheck for Stub {
        async fn already_ingested(&self, _id: Uuid) -> Result<bool, String> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn the_polls_cancel_and_its_failure_mode() {
        let companies = companies();
        let run = |stub: Option<Stub>| {
            let companies = companies.clone();
            async move {
                let mut seen = SeenSet::new(10);
                let check = stub.as_ref().map(|s| s as &dyn IngestedCheck);
                handle_message(GOMSPACE, at(1), &companies, &mut seen, check).await
            }
        };
        let (_, d) = run(Some(Stub(Ok(true)))).await;
        assert!(matches!(d.as_slice(), [Disposition::AlreadyIngested(_)]));
        let (_, d) = run(Some(Stub(Ok(false)))).await;
        assert!(matches!(d.as_slice(), [Disposition::New(_)]));
        // A failing lookup, or none at all, still logs the item.
        let (_, d) = run(Some(Stub(Err("db down".into())))).await;
        assert!(matches!(d.as_slice(), [Disposition::New(_)]));
        let (counters, d) = run(None).await;
        assert!(matches!(d.as_slice(), [Disposition::New(_)]));
        assert_eq!(
            counters,
            Counters {
                seen: 1,
                relevant: 1
            }
        );
    }
}
