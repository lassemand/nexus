//! The quote book: the latest quote `saxo-stream` streamed for each watched
//! instrument, held in memory so a price is read, not requested.
//!
//! Each instrument has a `tokio::sync::watch` channel. A reader takes the
//! current value without awaiting ([`QuoteBook::current`]) or waits for the next
//! update ([`QuoteBook::subscribe`] and `changed().await`). The stream task is
//! the only writer; no lock is held across an await on either side.
//!
//! The book also holds the stream's health and the watch list. The stream is
//! healthy while the WebSocket is connected, a `saxo-stream` heartbeat arrived
//! within the heartbeat timeout, and that heartbeat said `saxo_connected`.
//! While it is healthy, the latest quote is the market's current quote
//! whatever its age: Saxo only pushes on change.
//!
//! The watch list is the union of the scheduled instruments (replaced as a
//! whole by [`QuoteBook::set_scheduled`]) and the pinned ones (an instrument a
//! trade needs, held by a [`Pin`] until the trade ends).

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use tokio::sync::watch;

use crate::clock::Clock;
use crate::quotes::Quote;

/// A listing: upper-cased ticker and exchange MIC, as `saxo-stream` keys them.
pub type Pair = (String, String);

/// The book's key of `ticker` on `mic`.
pub fn key(ticker: &str, mic: &str) -> Pair {
    (ticker.trim().to_uppercase(), mic.trim().to_uppercase())
}

/// Where a price came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    /// Read from the quote book.
    Stream,
    /// Requested with `GET /quote`.
    Request,
}

impl Origin {
    /// The value stored in `*_quote_source` and used as the `source` label.
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Stream => "stream",
            Origin::Request => "request",
        }
    }

    /// Reads a stored value.
    pub fn parse(text: &str) -> Option<Origin> {
        match text {
            "stream" => Some(Origin::Stream),
            "request" => Some(Origin::Request),
            _ => None,
        }
    }
}

/// A streamed quote and where it stands in its connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The quote.
    pub quote: Quote,
    /// Saxo's timestamp, when readable.
    pub saxo_time: Option<DateTime<Utc>>,
    /// `saxo-stream`'s per-instrument sequence number.
    pub seq: u64,
    /// The connection it arrived on, so a restarted `saxo-stream` (whose
    /// sequence numbers start again) is not mistaken for old data.
    pub session: u64,
    /// When the analyst received it.
    pub received_at: DateTime<Utc>,
}

const NO_HEARTBEAT: i64 = i64::MIN;

struct Watched {
    scheduled: BTreeSet<Pair>,
    pins: HashMap<Pair, usize>,
}

impl Watched {
    fn union(&self) -> BTreeSet<Pair> {
        let mut all = self.scheduled.clone();
        all.extend(self.pins.keys().cloned());
        all
    }
}

/// The streamed quotes, the stream's health and the watch list.
pub struct QuoteBook {
    clock: Arc<dyn Clock>,
    heartbeat_timeout: ChronoDuration,
    quotes: RwLock<HashMap<Pair, watch::Sender<Option<Held>>>>,
    connected: AtomicBool,
    saxo_connected: AtomicBool,
    last_heartbeat_ms: AtomicI64,
    session: AtomicU64,
    watched: Mutex<Watched>,
    desired: watch::Sender<Arc<BTreeSet<Pair>>>,
}

impl QuoteBook {
    /// An empty book whose stream counts as unhealthy when no heartbeat arrived
    /// for `heartbeat_timeout`.
    pub fn new(clock: Arc<dyn Clock>, heartbeat_timeout: Duration) -> Arc<QuoteBook> {
        let (desired, _) = watch::channel(Arc::new(BTreeSet::new()));
        Arc::new(QuoteBook {
            clock,
            heartbeat_timeout: ChronoDuration::from_std(heartbeat_timeout)
                .unwrap_or(ChronoDuration::MAX),
            quotes: RwLock::new(HashMap::new()),
            connected: AtomicBool::new(false),
            saxo_connected: AtomicBool::new(false),
            last_heartbeat_ms: AtomicI64::new(NO_HEARTBEAT),
            session: AtomicU64::new(0),
            watched: Mutex::new(Watched {
                scheduled: BTreeSet::new(),
                pins: HashMap::new(),
            }),
            desired,
        })
    }

    /// A book nothing feeds: never healthy, so every price is requested.
    pub fn disabled(clock: Arc<dyn Clock>) -> Arc<QuoteBook> {
        QuoteBook::new(clock, Duration::from_secs(3))
    }

    /// Whether prices may be read from the book now.
    pub fn is_healthy(&self) -> bool {
        if !self.connected.load(Ordering::Acquire) || !self.saxo_connected.load(Ordering::Acquire) {
            return false;
        }
        let last = self.last_heartbeat_ms.load(Ordering::Acquire);
        if last == NO_HEARTBEAT {
            return false;
        }
        let age_ms = self.clock.now().timestamp_millis() - last;
        age_ms <= self.heartbeat_timeout.num_milliseconds()
    }

    /// The WebSocket opened: a new session starts, and nothing is confirmed
    /// until the first heartbeat.
    pub fn on_connected(&self) {
        self.session.fetch_add(1, Ordering::AcqRel);
        self.saxo_connected.store(false, Ordering::Release);
        self.last_heartbeat_ms
            .store(NO_HEARTBEAT, Ordering::Release);
        self.connected.store(true, Ordering::Release);
    }

    /// The WebSocket closed.
    pub fn on_disconnected(&self) {
        self.connected.store(false, Ordering::Release);
        self.saxo_connected.store(false, Ordering::Release);
        self.last_heartbeat_ms
            .store(NO_HEARTBEAT, Ordering::Release);
    }

    /// A `saxo-stream` heartbeat arrived.
    pub fn on_heartbeat(&self, saxo_connected: bool) {
        self.saxo_connected.store(saxo_connected, Ordering::Release);
        self.last_heartbeat_ms
            .store(self.clock.now().timestamp_millis(), Ordering::Release);
    }

    /// The current session number.
    pub fn session(&self) -> u64 {
        self.session.load(Ordering::Acquire)
    }

    /// Stores a streamed quote unless it is older than the one held on the same
    /// connection. Returns whether it was stored.
    pub fn apply(&self, pair: &Pair, quote: Quote, seq: u64) -> bool {
        let session = self.session();
        let held = Held {
            saxo_time: quote.saxo_time(),
            quote,
            seq,
            session,
            received_at: self.clock.now(),
        };
        let sender = self.sender(pair);
        sender.send_if_modified(|current| {
            if current
                .as_ref()
                .is_some_and(|old| old.session == session && seq < old.seq)
            {
                return false;
            }
            *current = Some(held);
            true
        })
    }

    fn sender(&self, pair: &Pair) -> watch::Sender<Option<Held>> {
        {
            let quotes = self.quotes.read().unwrap_or_else(|e| e.into_inner());
            if let Some(sender) = quotes.get(pair) {
                return sender.clone();
            }
        }
        let mut quotes = self.quotes.write().unwrap_or_else(|e| e.into_inner());
        quotes
            .entry(pair.clone())
            .or_insert_with(|| watch::channel(None).0)
            .clone()
    }

    /// The quote held for `pair` now. Never awaits.
    pub fn current(&self, pair: &Pair) -> Option<Held> {
        let quotes = self.quotes.read().unwrap_or_else(|e| e.into_inner());
        quotes.get(pair).and_then(|sender| sender.borrow().clone())
    }

    /// A receiver for `pair`'s updates; `changed().await` returns at the next
    /// one.
    pub fn subscribe(&self, pair: &Pair) -> watch::Receiver<Option<Held>> {
        self.sender(pair).subscribe()
    }

    /// Replaces the scheduled part of the watch list.
    pub fn set_scheduled(&self, scheduled: BTreeSet<Pair>) {
        let mut watched = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        if watched.scheduled == scheduled {
            return;
        }
        watched.scheduled = scheduled;
        self.publish_locked(&watched);
    }

    /// Adds `pair` to the watch list until the returned [`Pin`] is dropped.
    pub fn pin(self: &Arc<Self>, pair: Pair) -> Pin {
        let mut watched = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        *watched.pins.entry(pair.clone()).or_insert(0) += 1;
        self.publish_locked(&watched);
        Pin {
            book: Arc::clone(self),
            pair,
        }
    }

    fn unpin(&self, pair: &Pair) {
        let mut watched = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = watched.pins.get_mut(pair) {
            *count -= 1;
            if *count == 0 {
                watched.pins.remove(pair);
            }
        }
        self.publish_locked(&watched);
    }

    fn publish_locked(&self, watched: &Watched) {
        let union = watched.union();
        crate::metrics::watched_instruments(union.len());
        self.desired.send_if_modified(|current| {
            if **current == union {
                return false;
            }
            *current = Arc::new(union);
            true
        });
    }

    /// Whether `pair` is on the watch list.
    pub fn is_watched(&self, pair: &Pair) -> bool {
        let watched = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        watched.scheduled.contains(pair) || watched.pins.contains_key(pair)
    }

    /// The watch list now.
    pub fn watch_list(&self) -> Arc<BTreeSet<Pair>> {
        Arc::clone(&self.desired.borrow())
    }

    /// A receiver that fires whenever the watch list changes.
    pub fn watch_changes(&self) -> watch::Receiver<Arc<BTreeSet<Pair>>> {
        self.desired.subscribe()
    }
}

/// Keeps an instrument on the watch list while it is alive.
pub struct Pin {
    book: Arc<QuoteBook>,
    pair: Pair,
}

impl Drop for Pin {
    fn drop(&mut self) {
        self.book.unpin(&self.pair);
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::clock::ManualClock;

    fn pair(t: &str) -> Pair {
        key(t, "xsto")
    }

    fn quote(ask: &str, saxo: &str) -> Quote {
        Quote {
            ask: ask.parse().ok(),
            ask_type: Some("Tradable".into()),
            delayed_minutes: Some(0),
            saxo_last_updated: Some(saxo.into()),
            ..Quote::default()
        }
    }

    fn book() -> (Arc<ManualClock>, Arc<QuoteBook>) {
        let clock = Arc::new(ManualClock::new(
            Utc.with_ymd_and_hms(2026, 11, 10, 8, 0, 0).unwrap(),
        ));
        let book = QuoteBook::new(clock.clone(), Duration::from_secs(3));
        (clock, book)
    }

    #[test]
    fn keys_are_upper_cased_and_trimmed() {
        assert_eq!(key(" aaa ", "xsto"), ("AAA".into(), "XSTO".into()));
    }

    #[test]
    fn a_quote_is_read_without_awaiting_and_replaced_by_a_newer_one() {
        let (_, book) = book();
        assert_eq!(book.current(&pair("AAA")), None);
        assert!(book.apply(&pair("AAA"), quote("100.5", "2026-11-10T08:00:00Z"), 1));
        let held = book.current(&pair("AAA")).expect("held");
        assert_eq!(held.quote.ask, "100.5".parse().ok());
        assert_eq!(held.seq, 1);
        assert_eq!(
            held.saxo_time,
            Some(Utc.with_ymd_and_hms(2026, 11, 10, 8, 0, 0).unwrap())
        );
        assert!(book.apply(&pair("AAA"), quote("101", "2026-11-10T08:00:01Z"), 2));
        assert_eq!(book.current(&pair("AAA")).unwrap().seq, 2);
    }

    #[test]
    fn an_older_sequence_number_is_ignored_within_a_connection() {
        let (_, book) = book();
        book.on_connected();
        assert!(book.apply(&pair("AAA"), quote("101", "2026-11-10T08:00:01Z"), 5));
        assert!(!book.apply(&pair("AAA"), quote("100", "2026-11-10T08:00:00Z"), 4));
        assert_eq!(
            book.current(&pair("AAA")).unwrap().quote.ask,
            "101".parse().ok()
        );
        assert!(book.apply(&pair("AAA"), quote("102", "2026-11-10T08:00:02Z"), 5));
    }

    #[test]
    fn sequence_numbers_may_restart_on_a_new_connection() {
        let (_, book) = book();
        book.on_connected();
        assert!(book.apply(&pair("AAA"), quote("101", "2026-11-10T08:00:01Z"), 500));
        book.on_disconnected();
        book.on_connected();
        assert!(book.apply(&pair("AAA"), quote("99", "2026-11-10T08:05:00Z"), 1));
        assert_eq!(
            book.current(&pair("AAA")).unwrap().quote.ask,
            "99".parse().ok()
        );
    }

    #[tokio::test]
    async fn a_waiting_reader_wakes_on_the_next_update_only() {
        let (_, book) = book();
        book.apply(&pair("AAA"), quote("100", "2026-11-10T08:00:00Z"), 1);
        let mut rx = book.subscribe(&pair("AAA"));
        rx.borrow_and_update();
        let waiter = tokio::spawn(async move {
            rx.changed().await.expect("sender alive");
            rx.borrow().clone()
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        book.apply(&pair("AAA"), quote("101", "2026-11-10T08:00:01Z"), 2);
        let got = waiter.await.expect("task").expect("held");
        assert_eq!(got.seq, 2);
    }

    #[test]
    fn health_needs_a_connection_a_fresh_heartbeat_and_saxo_connected() {
        let (clock, book) = book();
        assert!(!book.is_healthy());
        book.on_connected();
        assert!(!book.is_healthy(), "no heartbeat yet");
        book.on_heartbeat(true);
        assert!(book.is_healthy());
        clock.set(clock.now() + ChronoDuration::milliseconds(2999));
        assert!(book.is_healthy());
        clock.set(clock.now() + ChronoDuration::milliseconds(2));
        assert!(!book.is_healthy(), "heartbeat older than the timeout");
        book.on_heartbeat(true);
        assert!(book.is_healthy());
        book.on_heartbeat(false);
        assert!(!book.is_healthy(), "saxo-stream says Saxo is down");
        book.on_heartbeat(true);
        book.on_disconnected();
        assert!(!book.is_healthy());
    }

    #[test]
    fn a_new_connection_is_unconfirmed_until_its_first_heartbeat() {
        let (_, book) = book();
        book.on_connected();
        book.on_heartbeat(true);
        assert!(book.is_healthy());
        book.on_disconnected();
        book.on_connected();
        assert!(!book.is_healthy());
    }

    #[test]
    fn the_watch_list_is_the_union_of_scheduled_and_pinned() {
        let (_, book) = book();
        let changes = book.watch_changes();
        book.set_scheduled([pair("AAA"), pair("BBB")].into());
        assert!(book.is_watched(&pair("AAA")));
        assert!(!book.is_watched(&pair("CCC")));
        let pin = book.pin(pair("CCC"));
        let second = book.pin(pair("CCC"));
        assert!(book.is_watched(&pair("CCC")));
        assert_eq!(book.watch_list().len(), 3);
        assert!(changes.has_changed().unwrap());
        drop(pin);
        assert!(book.is_watched(&pair("CCC")), "still pinned once");
        drop(second);
        assert!(!book.is_watched(&pair("CCC")));
        book.set_scheduled([pair("AAA")].into());
        assert_eq!(
            book.watch_list().iter().cloned().collect::<Vec<_>>(),
            vec![pair("AAA")]
        );
    }

    #[test]
    fn an_instrument_stays_while_either_rule_holds() {
        let (_, book) = book();
        book.set_scheduled([pair("AAA")].into());
        let pin = book.pin(pair("AAA"));
        book.set_scheduled(BTreeSet::new());
        assert!(book.is_watched(&pair("AAA")));
        drop(pin);
        assert!(!book.is_watched(&pair("AAA")));
    }

    #[test]
    fn an_unchanged_watch_list_does_not_notify() {
        let (_, book) = book();
        book.set_scheduled([pair("AAA")].into());
        let mut changes = book.watch_changes();
        changes.borrow_and_update();
        book.set_scheduled([pair("AAA")].into());
        let pin = book.pin(pair("AAA"));
        assert!(!changes.has_changed().unwrap());
        drop(pin);
        assert!(!changes.has_changed().unwrap());
    }
}
