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

use super::announcement::is_announcement_class;
use super::calendar_release::parse_calendar_release;
use super::classify::classify_item;
use super::db::Company;
use super::events::{announcement_event, company_key, release_event, EventSink};
use super::feed::{parse_document, NewsItem};
use super::live_emit::{EmitJob, Emitter};
use super::slack::NoticeKind;
use async_trait::async_trait;
use backon::{BackoffBuilder, ExponentialBuilder};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use metrics::{counter, gauge, histogram};
use model::generated::SpecialEvent;
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
    /// Display name from the registry (`companies.name`).
    pub name: Option<String>,
}

impl Registered {
    /// As a registry row, for the shared event builders.
    pub fn as_company(&self) -> Company {
        Company {
            ticker: self.ticker.clone(),
            exchange_mic: self.exchange_mic.clone(),
            mfn_slug: Some(self.mfn_slug.clone()),
            name: self.name.clone(),
            isin: None,
        }
    }
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
    ///
    /// When the item lists tickers, the company's own `MIC:TICKER` must be among
    /// them: two MFN companies can share a name and a slug alias (Metso/Neles),
    /// and a registry row pointing at the wrong one must not claim its items.
    pub fn find(&self, item: &NewsItem) -> Option<&Registered> {
        let found = self.by_slug.get(&item.author_slug).or_else(|| {
            item.author_slugs
                .iter()
                .find_map(|s| self.by_slug.get(s.as_str()))
        })?;
        let listed = item.author_tickers.is_empty()
            || super::ingest::ticker_listed(
                &found.exchange_mic,
                &found.ticker,
                &item.author_tickers,
            );
        listed.then_some(found)
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
    /// `2026-Q3` or `2026`, from the classifier.
    pub fiscal_period: Option<String>,
    pub is_correction: bool,
    pub calendar_update: bool,
    /// The registry's name for the company.
    pub company_name: Option<String>,
    /// MFN's `author.name`.
    pub author_name: Option<String>,
    pub title: String,
    pub url: Option<String>,
    pub attachment_urls: Vec<String>,
    /// The event the poll would publish for this item (reports and
    /// corrections); `None` for news and calendar notices.
    pub event: Option<SpecialEvent>,
    /// The `company_announcement` event for this item (the poll's builder), or
    /// `None` for reports, corrections and calendar items.
    pub announcement: Option<SpecialEvent>,
    /// What happened to the announcement: `queued`, `dropped` or `disabled`.
    /// `None` for items that are not announcements.
    pub emit: Option<&'static str>,
}

impl LiveItem {
    /// The Kafka key of this item's events, `ticker:exchange_mic`.
    pub fn key(&self) -> String {
        company_key(&Company {
            ticker: self.ticker.clone(),
            exchange_mic: self.exchange_mic.clone(),
            mfn_slug: None,
            name: None,
            isin: None,
        })
    }

    /// `received_at − publish_date` in milliseconds. Negative means clock skew
    /// and is kept as is.
    pub fn latency_ms(&self) -> i64 {
        (self.received_at - self.publish_date).num_milliseconds()
    }

    /// True if the item was published more than `max_age` before it was read,
    /// so it must not be posted to Slack (a catch-up after a long outage).
    pub fn is_stale(&self, max_age: Duration) -> bool {
        self.latency_ms() > i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX)
    }

    /// `report_released` for results releases, `company_news` for the rest.
    pub fn event(&self) -> &'static str {
        if self.report_kind.is_some() {
            "report_released"
        } else {
            "company_news"
        }
    }

    /// True for results releases that go to Slack: not corrections, not company news.
    pub fn is_slack_candidate(&self) -> bool {
        self.report_kind.is_some() && !self.is_correction
    }

    /// The name shown in Slack: the registry name without a trailing
    /// ` (TICKER)` (the message adds the ticker itself), else MFN's author name,
    /// else the ticker.
    pub fn display_name(&self) -> String {
        let suffix = format!(" ({})", self.ticker);
        [self.company_name.as_deref(), self.author_name.as_deref()]
            .into_iter()
            .flatten()
            .map(|n| n.trim())
            .map(|n| n.strip_suffix(suffix.as_str()).unwrap_or(n).trim())
            .find(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.ticker.clone())
    }

    /// Logs the item once, as one structured event.
    pub fn log(&self) {
        self.log_with(None);
    }

    /// Like [`LiveItem::log`], with the `slack` field (`disabled` or `queued`)
    /// when Slack posting applies to this item.
    pub fn log_with(&self, slack: Option<&str>) {
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
            slack = slack.unwrap_or(""),
            emit = self.emit.unwrap_or(""),
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
    /// The newest `publish_date` of every parsed item, registered company or
    /// not: the live watermark.
    pub newest_published: Option<DateTime<Utc>>,
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
    pick_relevant(&mut out, doc.items, received_at, companies, seen);
    out
}

/// [`process_message`] for items that were not read from a socket message (the
/// catch-up's HTTP feed): the same filtering, de-duplication and classification.
pub fn process_items(
    items: Vec<NewsItem>,
    received_at: DateTime<Utc>,
    companies: &Companies,
    seen: &mut SeenSet,
) -> Processed {
    let mut out = Processed::default();
    pick_relevant(&mut out, items, received_at, companies, seen);
    out
}

fn pick_relevant(
    out: &mut Processed,
    items: Vec<NewsItem>,
    received_at: DateTime<Utc>,
    companies: &Companies,
    seen: &mut SeenSet,
) {
    for item in items {
        out.items_seen += 1;
        out.newest_published = out.newest_published.max(Some(item.published_at));
        let Some(company) = companies.find(&item) else {
            continue;
        };
        if !seen.insert(item.news_id) {
            out.duplicates += 1;
            continue;
        }
        // The same rules as the hourly poll (NEX-166).
        let c = classify_item(&item.tags, &item.title);
        let event = release_event(&company.as_company(), &item, &c);
        let announcement = is_announcement_class(&c, parse_calendar_release(&item).is_some())
            .then(|| announcement_event(&company.as_company(), &item));
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
            fiscal_period: c.fiscal_period,
            is_correction: c.is_correction,
            calendar_update: c.calendar_update,
            company_name: company.name.clone(),
            author_name: item.author_name,
            title: item.title,
            url: item.url,
            attachment_urls: item.attachment_urls,
            event,
            announcement,
            emit: None,
        });
    }
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
            Err(e) => {
                counter!("nordic_live_signal_check_failures_total").increment(1);
                warn!(news_id = %item.news_id, error = %e,
                      "signal database lookup failed, treating the item as new");
            }
        }
    }
    Disposition::New(Box::new(item))
}

/// The Slack channel IDs of the two kinds of post. A kind without a channel is not posted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlackChannels {
    /// Where results releases go.
    pub reports: Option<String>,
    /// Where company announcements go.
    pub announcements: Option<String>,
}

impl SlackChannels {
    /// Channels from configuration; empty strings count as unset.
    pub fn new(reports: Option<String>, announcements: Option<String>) -> Self {
        let set = |c: Option<String>| c.filter(|c| !c.is_empty());
        Self {
            reports: set(reports),
            announcements: set(announcements),
        }
    }

    /// The channel for `kind`.
    pub fn for_kind(&self, kind: NoticeKind) -> Option<&str> {
        match kind {
            NoticeKind::Report => self.reports.as_deref(),
            NoticeKind::Announcement => self.announcements.as_deref(),
        }
    }
}

/// Logs a disposition: the item itself, or `already ingested by poll`.
pub fn report(d: &Disposition) {
    report_with(d, &SlackChannels::default())
}

/// [`report`] with the channels: results releases and announcements get
/// `slack = "disabled"` or `"queued"` by whether their kind has a channel; the
/// worker logs the outcome in its own `slack_post` event.
pub fn report_with(d: &Disposition, channels: &SlackChannels) {
    report_labelled(d, channels, false)
}

/// [`report_with`], where a `stale` item (too old to post, see
/// [`LiveItem::is_stale`]) is labelled `slack = "stale"` when its kind has a channel.
pub fn report_labelled(d: &Disposition, channels: &SlackChannels, stale: bool) {
    let label = |kind| {
        if channels.for_kind(kind).is_none() {
            "disabled"
        } else if stale {
            "stale"
        } else {
            "queued"
        }
    };
    match d {
        Disposition::New(item) if item.is_slack_candidate() => {
            item.log_with(Some(label(NoticeKind::Report)))
        }
        Disposition::New(item) if item.announcement.is_some() => {
            item.log_with(Some(label(NoticeKind::Announcement)))
        }
        Disposition::New(item) => item.log(),
        Disposition::AlreadyIngested(id) => info!(news_id = %id, "already ingested by poll"),
    }
}

// ── Publishing ──────────────────────────────────────────────────────────────

/// The `special.events` messages for new items, keyed like the poll's
/// (`ticker:exchange_mic`). Items the poll already stored are skipped.
pub fn events_to_publish(dispositions: &[Disposition]) -> Vec<(String, SpecialEvent)> {
    dispositions
        .iter()
        .filter_map(|d| match d {
            Disposition::New(item) => item.event.clone().map(|e| {
                let key = company_key(&Company {
                    ticker: item.ticker.clone(),
                    exchange_mic: item.exchange_mic.clone(),
                    mfn_slug: None,
                    name: None,
                    isin: None,
                });
                (key, e)
            }),
            Disposition::AlreadyIngested(_) => None,
        })
        .collect()
}

/// Sets `nordic_live_events_published_total` to 0 for each result.
pub fn describe_publish_metrics() {
    for result in ["published", "failed"] {
        counter!("nordic_live_events_published_total", "result" => result).absolute(0);
    }
}

/// Publishes one event to `special.events`. A failure is logged and counted,
/// not returned: the nightly poll publishes the same item again.
pub async fn publish_live(
    sink: &(dyn EventSink + Send + Sync),
    key: &str,
    event: &SpecialEvent,
) -> bool {
    match sink.send(key, event).await {
        Ok(()) => {
            counter!("nordic_live_events_published_total", "result" => "published").increment(1);
            info!(key, news_id = %event.source_id, event_type = %event.event_type,
                  "published to special.events");
            true
        }
        Err(e) => {
            counter!("nordic_live_events_published_total", "result" => "failed").increment(1);
            warn!(key, news_id = %event.source_id, error = format!("{e:#}"),
                  "publishing to special.events failed; the poll will publish it");
            false
        }
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

/// Histogram buckets (seconds) for `nordic_live_release_latency_seconds`.
pub const LATENCY_BUCKETS: &[f64] = &[1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 300.0, 1800.0];

/// Registers every `nordic_live_*` series at zero so rates and alerts see them
/// before the first event. Call once after the recorder is installed.
pub fn describe_metrics() {
    for reason in ["server_close_3000", "error", "timeout", "other"] {
        counter!("nordic_live_reconnects_total", "reason" => reason).absolute(0);
    }
    for relevance in ["relevant", "other"] {
        counter!("nordic_live_items_total", "relevance" => relevance).absolute(0);
    }
    for event in ["report_released", "company_news"] {
        counter!("nordic_live_releases_total", "event" => event).absolute(0);
    }
    for name in [
        "nordic_live_messages_total",
        "nordic_live_message_parse_errors_total",
        "nordic_live_already_ingested_total",
        "nordic_live_negative_latency_total",
        "nordic_live_company_reload_failures_total",
        "nordic_live_signal_check_failures_total",
    ] {
        counter!(name).absolute(0);
    }
    gauge!("nordic_live_connected").set(0.0);
}

/// Counts the message's parsed items as relevant (registered company) or other.
fn record_items(p: &Processed) {
    let relevant = p.relevant.len() + p.duplicates;
    counter!("nordic_live_items_total", "relevance" => "relevant").increment(relevant as u64);
    counter!("nordic_live_items_total", "relevance" => "other")
        .increment(p.items_seen.saturating_sub(relevant) as u64);
}

/// Records receive-time minus publish-time. Negative values (clock skew) are
/// clamped to 0 and counted separately.
fn record_latency(latency_ms: i64) {
    if latency_ms < 0 {
        counter!("nordic_live_negative_latency_total").increment(1);
    }
    histogram!("nordic_live_release_latency_seconds").record(latency_ms.max(0) as f64 / 1000.0);
}

/// Slack posts older than this are skipped (default `SLACK_MAX_AGE_MINUTES`).
pub const DEFAULT_SLACK_MAX_AGE: Duration = Duration::from_secs(15 * 60);

static NO_CHANNELS: SlackChannels = SlackChannels {
    reports: None,
    announcements: None,
};

/// What happens to relevant items beyond the log line.
#[derive(Clone, Copy)]
pub struct Handling<'a> {
    /// The poll's "already ingested" check, if any.
    pub check: Option<&'a dyn IngestedCheck>,
    /// The Slack channels of the two kinds of post.
    pub channels: &'a SlackChannels,
    /// Where announcements are queued for `special.events`, if emission is on.
    pub emitter: Option<&'a Emitter>,
    /// Releases and announcements older than this are never posted to Slack.
    pub slack_max_age: Duration,
}

impl Default for Handling<'_> {
    fn default() -> Self {
        Self {
            check: None,
            channels: &NO_CHANNELS,
            emitter: None,
            slack_max_age: DEFAULT_SLACK_MAX_AGE,
        }
    }
}

/// What handling a message or a catch-up batch produced.
pub struct Handled {
    /// Items seen and relevant.
    pub counters: Counters,
    /// One disposition per new relevant item.
    pub dispositions: Vec<Disposition>,
    /// The newest `publish_date` of any parsed item.
    pub newest_published: Option<DateTime<Utc>>,
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
    handle_message_with(
        msg,
        received_at,
        companies,
        seen,
        check,
        &SlackChannels::default(),
    )
    .await
}

/// [`handle_message`] that also tells the log lines whether Slack is on.
pub async fn handle_message_with(
    msg: &str,
    received_at: DateTime<Utc>,
    companies: &Companies,
    seen: &mut SeenSet,
    check: Option<&dyn IngestedCheck>,
    channels: &SlackChannels,
) -> (Counters, Vec<Disposition>) {
    let handling = Handling {
        check,
        channels,
        ..Handling::default()
    };
    let h = handle_message_full(msg, received_at, companies, seen, &handling).await;
    (h.counters, h.dispositions)
}

/// Handles one socket message with everything `nordic-live` does: queues each
/// item's announcement event on the emitter right after parsing (before the
/// "already ingested" lookup, so neither that database nor Slack can delay or
/// suppress it), then looks up, logs and returns the dispositions.
pub async fn handle_message_full(
    msg: &str,
    received_at: DateTime<Utc>,
    companies: &Companies,
    seen: &mut SeenSet,
    h: &Handling<'_>,
) -> Handled {
    let processed = process_message(msg, received_at, companies, seen);
    counter!("nordic_live_messages_total").increment(1);
    gauge!("nordic_live_last_message_timestamp_seconds")
        .set(received_at.timestamp_millis() as f64 / 1000.0);
    if processed.malformed.is_some() {
        counter!("nordic_live_message_parse_errors_total").increment(1);
    }
    if let Some(why) = &processed.malformed {
        let head: String = msg.chars().take(300).collect();
        warn!(error = %why, message = %head, "malformed nordic-live message, skipped");
    }
    for why in &processed.bad_items {
        let head: String = msg.chars().take(300).collect();
        warn!(error = %why, message = %head, "unusable item in nordic-live message, skipped");
    }
    record_items(&processed);
    dispatch(processed, h, true).await
}

/// Handles items fetched by the catch-up through exactly the path of a socket
/// message (filter, de-duplicate, classify, emit, look up, log). `items` must
/// be oldest first so a company's events stay in order on Kafka. Their
/// receive-to-publish gap is not a release latency, so it is not recorded.
pub async fn handle_catchup_items(
    items: Vec<NewsItem>,
    received_at: DateTime<Utc>,
    companies: &Companies,
    seen: &mut SeenSet,
    h: &Handling<'_>,
) -> Handled {
    let processed = process_items(items, received_at, companies, seen);
    dispatch(processed, h, false).await
}

async fn dispatch(mut processed: Processed, h: &Handling<'_>, from_socket: bool) -> Handled {
    for item in &mut processed.relevant {
        if let Some(event) = item.announcement.clone() {
            item.emit = Some(match h.emitter {
                Some(e) => e.offer(EmitJob {
                    key: item.key(),
                    event,
                    received_at: item.received_at,
                }),
                None => "disabled",
            });
        }
    }
    let counters = Counters {
        seen: processed.items_seen,
        relevant: processed.relevant.len() + processed.duplicates,
    };
    if !from_socket {
        counter!("nordic_live_catchup_items_total").increment(processed.relevant.len() as u64);
    }
    let newest_published = processed.newest_published;
    let mut dispositions = Vec::new();
    for item in processed.relevant {
        if from_socket {
            record_latency(item.latency_ms());
        }
        let stale = item.is_stale(h.slack_max_age);
        let d = dispose(item, h.check).await;
        match &d {
            Disposition::New(item) => {
                counter!("nordic_live_releases_total", "event" => item.event()).increment(1)
            }
            Disposition::AlreadyIngested(_) => {
                counter!("nordic_live_already_ingested_total").increment(1)
            }
        }
        report_labelled(&d, h.channels, stale);
        dispositions.push(d);
    }
    Handled {
        counters,
        dispositions,
        newest_published,
    }
}

// ── Connection ──────────────────────────────────────────────────────────────

/// Reconnect delay: 10 ms, doubling, jittered. backon adds the jitter after
/// the cap (each wait is `d + d × rand[0,1)`), so the cap is 15 s and the
/// longest wait stays under the 30 s MFN's own site uses.
pub const fn reconnect_backoff() -> ExponentialBuilder {
    ExponentialBuilder::new()
        .with_min_delay(Duration::from_millis(10))
        .with_factor(2.0)
        .with_max_delay(Duration::from_secs(15))
        .without_max_times()
        .with_jitter()
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

/// Why a session failed, as the `reason` label of `nordic_live_reconnects_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailKind {
    Error,
    Timeout,
    Other,
}

impl FailKind {
    fn label(self) -> &'static str {
        match self {
            FailKind::Error => "error",
            FailKind::Timeout => "timeout",
            FailKind::Other => "other",
        }
    }
}

enum SessionEnd {
    FastReconnect,
    Failed(FailKind, String),
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
    /// Delays between reconnects; tests seed its jitter.
    pub reconnect: ExponentialBuilder,
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
            reconnect: reconnect_backoff(),
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
        let mut backoff = self.reconnect.build();
        loop {
            if tx.is_closed() {
                return;
            }
            let (end, up_for) = self.session(&tx).await;
            gauge!("nordic_live_connected").set(0.0);
            match end {
                SessionEnd::ReceiverGone => return,
                SessionEnd::FastReconnect => {
                    counter!("nordic_live_reconnects_total", "reason" => "server_close_3000")
                        .increment(1);
                    debug!("MFN closed the connection with 3000, reconnecting");
                    self.emit(ConnEvent::FastReconnect);
                    if up_for.is_some_and(|d| d >= self.stable_after) {
                        backoff = self.reconnect.build();
                    }
                }
                SessionEnd::Failed(kind, reason) => {
                    counter!("nordic_live_reconnects_total", "reason" => kind.label()).increment(1);
                    if up_for.is_some_and(|d| d >= self.stable_after) {
                        backoff = self.reconnect.build();
                    }
                    let delay = backoff.next().unwrap_or(Duration::from_secs(30));
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
            Err(e) => {
                return (
                    SessionEnd::Failed(FailKind::Error, format!("bad url: {e}")),
                    None,
                )
            }
        };
        if let Ok(ua) = self.user_agent.parse() {
            request.headers_mut().insert(USER_AGENT, ua);
        }
        let connect = tokio_tungstenite::connect_async(request);
        let mut ws = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => {
                return (
                    SessionEnd::Failed(FailKind::Error, format!("connect failed: {e}")),
                    None,
                )
            }
            Err(_) => {
                return (
                    SessionEnd::Failed(FailKind::Timeout, "connect timed out".to_string()),
                    None,
                )
            }
        };
        info!(url = %self.url, "connected to MFN");
        self.emit(ConnEvent::Connected);
        gauge!("nordic_live_connected").set(1.0);
        let up_since = Instant::now();

        loop {
            // Any frame, including ping and pong, shows the peer is alive.
            let next = match tokio::time::timeout(self.dead_after, ws.next()).await {
                Ok(n) => n,
                Err(_) => {
                    warn!(silent_for = ?self.dead_after, "no message or ping from MFN, dropping the connection");
                    self.emit(ConnEvent::Dead);
                    return (
                        SessionEnd::Failed(
                            FailKind::Timeout,
                            "no message or ping/pong".to_string(),
                        ),
                        Some(up_since.elapsed()),
                    );
                }
            };
            match next {
                None => {
                    return (
                        SessionEnd::Failed(FailKind::Error, "stream ended".to_string()),
                        Some(up_since.elapsed()),
                    )
                }
                Some(Err(e)) => {
                    return (
                        SessionEnd::Failed(FailKind::Error, e.to_string()),
                        Some(up_since.elapsed()),
                    )
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
                            SessionEnd::Failed(
                                FailKind::Other,
                                format!("closed by server: {other:?}"),
                            ),
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
            name: None,
        }])
    }

    fn at(secs_after: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 26, 6, 0, 0).unwrap() + chrono::Duration::seconds(secs_after)
    }

    #[test]
    fn finnish_tickers_match_and_a_namesake_entity_is_refused() {
        let companies = Companies::new(vec![
            Registered {
                ticker: "METSO".into(),
                exchange_mic: "XHEL".into(),
                mfn_slug: "metso-2".into(),
                name: None,
            },
            Registered {
                ticker: "WRONG".into(),
                exchange_mic: "XHEL".into(),
                // A misconfigured row on the old Metso / Neles slug.
                mfn_slug: "metso".into(),
                name: None,
            },
        ]);
        let item = |slug: &str, slugs: &[&str], tickers: &[&str]| NewsItem {
            news_id: Uuid::nil(),
            url: None,
            lang: Some("en".into()),
            tags: vec![],
            title: "t".into(),
            published_at: Utc::now(),
            author_slug: slug.into(),
            author_name: None,
            author_slugs: slugs.iter().map(|s| s.to_string()).collect(),
            author_tickers: tickers.iter().map(|s| s.to_string()).collect(),
            author_isins: vec![],
            source: None,
            attachment_urls: vec![],
            text: None,
        };
        let metso = item(
            "metso-2",
            &["metso-2", "metso-outotec", "outotec"],
            &["XHEL:METSO", "XLON:0MGI"],
        );
        assert_eq!(
            companies.find(&metso).map(|c| c.ticker.as_str()),
            Some("METSO")
        );
        // The Neles entity answers to the slug `metso` but lists XHEL:NELES.
        let neles = item(
            "neles",
            &["metso", "metso-corporation", "neles"],
            &["XHEL:NELES"],
        );
        assert!(companies.find(&neles).is_none());
        // First North Finland items match on their own MIC.
        let fnfi = Companies::new(vec![Registered {
            ticker: "ABC".into(),
            exchange_mic: "FNFI".into(),
            mfn_slug: "abc".into(),
            name: None,
        }]);
        assert!(fnfi.find(&item("abc", &["abc"], &["FNFI:ABC"])).is_some());
        assert!(fnfi.find(&item("abc", &["abc"], &["XHEL:ABC"])).is_none());
        // A WebSocket item without tickers is matched by slug alone, as before.
        assert!(fnfi.find(&item("abc", &["abc"], &[])).is_some());
    }

    /// The GomSpace fixture rewritten as business news (no report tags).
    fn as_news(msg: &str) -> String {
        let mut doc: serde_json::Value = serde_json::from_str(msg).unwrap();
        let item = &mut doc["items"][0];
        item["properties"]["tags"] = serde_json::json!([":regulatory"]);
        item["content"]["title"] = serde_json::json!("GomSpace signs a new customer contract");
        item["news_id"] = serde_json::json!("6e343aa5-f961-55a9-a00e-a94cf5d4d401");
        doc.to_string()
    }

    #[derive(Default)]
    struct VecSink {
        sent: std::sync::Mutex<Vec<(String, SpecialEvent)>>,
        fail: bool,
    }

    #[async_trait]
    impl EventSink for VecSink {
        async fn send(&self, key: &str, event: &SpecialEvent) -> anyhow::Result<()> {
            if self.fail {
                anyhow::bail!("broker unavailable");
            }
            self.sent
                .lock()
                .unwrap()
                .push((key.to_string(), event.clone()));
            Ok(())
        }
    }

    /// The GomSpace fixture with its tags, title, id and text replaced.
    fn variant(msg: &str, id: &str, tags: &[&str], title: &str, text: &str) -> String {
        let mut doc: serde_json::Value = serde_json::from_str(msg).unwrap();
        let item = &mut doc["items"][0];
        item["properties"]["tags"] = serde_json::json!(tags);
        item["content"]["title"] = serde_json::json!(title);
        item["content"]["text"] = serde_json::json!(text);
        item["news_id"] = serde_json::json!(id);
        doc.to_string()
    }

    const WARNING_ID: &str = "7e343aa5-f961-55a9-a00e-a94cf5d4d401";

    fn warning(id: &str) -> String {
        variant(
            GOMSPACE,
            id,
            &[":regulatory"],
            "GomSpace Group AB: Downward adjustment of guidance for 2026",
            "  The board lowers its outlook.  ",
        )
    }

    #[test]
    fn a_profit_warning_carries_exactly_the_event_the_poll_would_build() {
        let msg = warning(WARNING_ID);
        let mut seen = SeenSet::new(10);
        let p = process_message(&msg, at(3), &companies(), &mut seen);
        assert_eq!(p.relevant.len(), 1);
        let item = parse_document(&msg).unwrap().items.remove(0);
        let registered = companies().find(&item).unwrap().as_company();
        let expected = announcement_event(&registered, &item);
        let got = p.relevant[0]
            .announcement
            .as_ref()
            .expect("an announcement");
        assert_eq!(got, &expected);
        assert_eq!(got.event_type, "company_announcement");
        assert_eq!(got.text, "The board lowers its outlook.");
        assert_eq!(got.source_id, WARNING_ID);
        assert!(p.relevant[0].event.is_none());
        assert_eq!(p.relevant[0].key(), "GOMX:FNSE");
    }

    #[test]
    fn reports_corrections_and_calendar_releases_are_not_announcements() {
        let mut seen = SeenSet::new(10);
        let report = process_message(GOMSPACE, at(3), &companies(), &mut seen);
        assert!(report.relevant[0].announcement.is_none());
        let correction = variant(
            GOMSPACE,
            "8e343aa5-f961-55a9-a00e-a94cf5d4d401",
            &[":regulatory", "sub:report", "sub:report:interim"],
            "GomSpace Group AB: Correction: Interim report Q2 2026",
            "Corrected.",
        );
        let c = process_message(&correction, at(3), &companies(), &mut seen);
        assert!(c.relevant[0].is_correction && c.relevant[0].announcement.is_none());
        let calendar = variant(
            GOMSPACE,
            "9e343aa5-f961-55a9-a00e-a94cf5d4d401",
            &[":regulatory", "sub:ci", "sub:ci:calendar"],
            "Financial calendar 2026 for GomSpace Group AB",
            "27 May 2026 Interim report Q1 2026",
        );
        let cal = process_message(&calendar, at(3), &companies(), &mut seen);
        assert_eq!(cal.relevant.len(), 1);
        assert!(cal.relevant[0].announcement.is_none());
    }

    #[tokio::test]
    async fn the_reader_never_waits_on_a_full_queue_and_later_items_are_emitted() {
        let (emitter, mut rx) = Emitter::channel(1);
        let mut seen = SeenSet::new(10);
        let ids = [
            "7e343aa5-f961-55a9-a00e-a94cf5d4d401",
            "7e343aa5-f961-55a9-a00e-a94cf5d4d402",
            "7e343aa5-f961-55a9-a00e-a94cf5d4d403",
        ];
        let mut statuses = Vec::new();
        for (n, id) in ids.iter().enumerate() {
            if n == 2 {
                assert_eq!(rx.recv().await.unwrap().event.source_id, ids[0]);
            }
            let ds = handle_message_full(
                &warning(id),
                at(3),
                &companies(),
                &mut seen,
                &Handling {
                    emitter: Some(&emitter),
                    ..Handling::default()
                },
            )
            .await
            .dispositions;
            match &ds[0] {
                Disposition::New(i) => statuses.push(i.emit),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            statuses,
            vec![Some("queued"), Some("dropped"), Some("queued")]
        );
        assert_eq!(rx.recv().await.unwrap().event.source_id, ids[2]);
    }

    #[tokio::test]
    async fn emission_is_independent_of_the_ingested_check_and_can_be_off() {
        struct Always;
        #[async_trait]
        impl IngestedCheck for Always {
            async fn already_ingested(&self, _id: Uuid) -> Result<bool, String> {
                Ok(true)
            }
        }
        let (emitter, mut rx) = Emitter::channel(4);
        let mut seen = SeenSet::new(10);
        let ds = handle_message_full(
            &warning(WARNING_ID),
            at(3),
            &companies(),
            &mut seen,
            &Handling {
                check: Some(&Always),
                emitter: Some(&emitter),
                ..Handling::default()
            },
        )
        .await
        .dispositions;
        assert!(matches!(ds[0], Disposition::AlreadyIngested(_)));
        assert_eq!(rx.try_recv().unwrap().event.source_id, WARNING_ID);

        let ds = handle_message_full(
            &warning("7e343aa5-f961-55a9-a00e-a94cf5d4d409"),
            at(3),
            &companies(),
            &mut seen,
            &Handling::default(),
        )
        .await
        .dispositions;
        match &ds[0] {
            Disposition::New(i) => assert_eq!(i.emit, Some("disabled")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_report_carries_the_event_the_poll_would_publish() {
        let mut seen = SeenSet::new(10);
        let p = process_message(GOMSPACE, at(3), &companies(), &mut seen);
        let e = p.relevant[0].event.as_ref().expect("a report has an event");
        assert_eq!(e.event_type, "quarterly_report");
        assert_eq!(
            (e.ticker.as_str(), e.exchange_mic.as_str()),
            ("GOMX", "FNSE")
        );
        assert_eq!(e.source, "mfn");
        assert_eq!(e.source_id, "5e343aa5-f961-55a9-a00e-a94cf5d4d401");
        assert_eq!(e.report_kind, "interim");
        assert_eq!(
            e.occurred_at_unix_secs,
            p.relevant[0].publish_date.timestamp()
        );

        let news = process_message(&as_news(GOMSPACE), at(3), &companies(), &mut seen);
        assert_eq!(news.relevant.len(), 1);
        assert!(news.relevant[0].event.is_none(), "news is not published");
    }

    #[tokio::test]
    async fn only_new_items_with_an_event_are_published() {
        let mut seen = SeenSet::new(10);
        let report = process_message(GOMSPACE, at(3), &companies(), &mut seen)
            .relevant
            .remove(0);
        let news = process_message(&as_news(GOMSPACE), at(3), &companies(), &mut seen)
            .relevant
            .remove(0);
        let ds = vec![
            Disposition::New(Box::new(report.clone())),
            Disposition::New(Box::new(news)),
            Disposition::AlreadyIngested(report.news_id),
        ];
        let out = events_to_publish(&ds);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "GOMX:FNSE");

        let sink = VecSink::default();
        assert!(publish_live(&sink, &out[0].0, &out[0].1).await);
        assert_eq!(sink.sent.lock().unwrap().len(), 1);

        let down = VecSink {
            fail: true,
            ..Default::default()
        };
        assert!(
            !publish_live(&down, &out[0].0, &out[0].1).await,
            "a failure is reported, not raised"
        );
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
            name: None,
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
    fn backoff_doubles_from_10ms_with_jitter_and_stays_under_30s() {
        let delays: Vec<Duration> = reconnect_backoff().build().take(14).collect();
        for (i, d) in delays.iter().enumerate() {
            let base = Duration::from_millis(10 << i).min(Duration::from_secs(15));
            assert!(
                *d >= base && *d < base * 2,
                "delay {i} = {d:?}, base {base:?}"
            );
        }
        assert!(delays.iter().all(|d| *d < Duration::from_secs(30)));
    }

    #[test]
    fn a_seeded_backoff_is_repeatable() {
        let a: Vec<Duration> = reconnect_backoff()
            .with_jitter_seed(7)
            .build()
            .take(5)
            .collect();
        let b: Vec<Duration> = reconnect_backoff()
            .with_jitter_seed(7)
            .build()
            .take(5)
            .collect();
        assert_eq!(a, b);
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

    /// Runs `f` against a fresh local recorder and returns the rendered metrics.
    async fn metered<F: std::future::Future>(f: F) -> String {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new()
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Full(
                    "nordic_live_release_latency_seconds".into(),
                ),
                LATENCY_BUCKETS,
            )
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);
        f.await;
        handle.render()
    }

    /// The value of an exact series line such as `name{label="x"}`.
    fn value(text: &str, series: &str) -> f64 {
        text.lines()
            .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
            .unwrap_or_else(|| panic!("series {series} not found in:\n{text}"))
            .parse()
            .unwrap()
    }

    async fn handle(msg: &str, received: DateTime<Utc>, check: Option<&dyn IngestedCheck>) {
        let mut seen = SeenSet::new(10);
        handle_message(msg, received, &companies(), &mut seen, check).await;
    }

    #[tokio::test]
    async fn counts_a_relevant_item() {
        let text = metered(handle(GOMSPACE, at(3), None)).await;
        assert_eq!(value(&text, "nordic_live_messages_total"), 1.0);
        assert_eq!(
            value(&text, "nordic_live_items_total{relevance=\"relevant\"}"),
            1.0
        );
        assert_eq!(
            value(&text, "nordic_live_items_total{relevance=\"other\"}"),
            0.0
        );
        assert_eq!(
            value(&text, "nordic_live_release_latency_seconds_count"),
            1.0
        );
        assert_eq!(value(&text, "nordic_live_release_latency_seconds_sum"), 3.0);
        assert_eq!(
            value(
                &text,
                "nordic_live_release_latency_seconds_bucket{le=\"2\"}"
            ),
            0.0
        );
        assert_eq!(
            value(
                &text,
                "nordic_live_release_latency_seconds_bucket{le=\"5\"}"
            ),
            1.0
        );
        assert!(
            value(&text, "nordic_live_last_message_timestamp_seconds") > 1.7e9,
            "{text}"
        );
        let released = text
            .lines()
            .filter(|l| l.starts_with("nordic_live_releases_total{"))
            .count();
        assert_eq!(released, 1, "one event label: {text}");
    }

    #[tokio::test]
    async fn counts_an_item_from_an_unregistered_company_as_other() {
        let text = metered(handle(VOLVO, at(3), None)).await;
        assert_eq!(value(&text, "nordic_live_messages_total"), 1.0);
        assert_eq!(
            value(&text, "nordic_live_items_total{relevance=\"other\"}"),
            1.0
        );
        assert!(
            !text.contains("nordic_live_release_latency_seconds"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn counts_a_malformed_message() {
        let text = metered(handle("not json", at(3), None)).await;
        assert_eq!(value(&text, "nordic_live_messages_total"), 1.0);
        assert_eq!(value(&text, "nordic_live_message_parse_errors_total"), 1.0);
    }

    #[tokio::test]
    async fn counts_an_item_the_poll_already_stored() {
        let stub = Stub(Ok(true));
        let text = metered(handle(GOMSPACE, at(3), Some(&stub))).await;
        assert_eq!(value(&text, "nordic_live_already_ingested_total"), 1.0);
        assert!(!text.contains("nordic_live_releases_total"), "{text}");
    }

    #[tokio::test]
    async fn counts_a_failed_signal_check_and_still_releases() {
        let stub = Stub(Err("db down".into()));
        let text = metered(handle(GOMSPACE, at(3), Some(&stub))).await;
        assert_eq!(value(&text, "nordic_live_signal_check_failures_total"), 1.0);
        assert!(text.contains("nordic_live_releases_total{event="), "{text}");
    }

    #[tokio::test]
    async fn negative_latency_is_clamped_to_zero_and_counted() {
        // Received 5 s before MFN's publish_date.
        let text = metered(handle(GOMSPACE, at(-5), None)).await;
        assert_eq!(value(&text, "nordic_live_negative_latency_total"), 1.0);
        assert_eq!(value(&text, "nordic_live_release_latency_seconds_sum"), 0.0);
        assert_eq!(
            value(
                &text,
                "nordic_live_release_latency_seconds_bucket{le=\"1\"}"
            ),
            1.0
        );
    }
}
