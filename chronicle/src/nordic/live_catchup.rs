//! Catch-up for `nordic-live` after every (re)connect (NEX-242).
//!
//! MFN's WebSocket has no replay, so an item published while the socket is
//! down is never sent. After each successful connect a catch-up reads MFN's
//! HTTP feed of the same stream (`https://mfn.se/all/s/nordic.json`, newest
//! first, `next_url` paging) back to the *watermark* (the newest `publish_date`
//! processed so far, from the socket or an earlier catch-up) minus a 2 minute
//! overlap, and hands the items, oldest first, to the main loop, which runs
//! them through exactly the path of a socket item. `SeenSet` drops what the
//! socket already delivered.
//!
//! The fetching runs in its own task: the socket never waits for it.

use super::calendar::MfnClient;
use super::feed::{fetch_feed_page, NewsItem};
use super::live::ConnEvent;
use backon::{ExponentialBuilder, Retryable};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use metrics::{counter, gauge};
use sqlx::PgPool;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// MFN's HTTP feed of the whole Nordic stream.
pub const DEFAULT_FEED_URL: &str = "https://mfn.se/all/s/nordic.json";
/// Items older than the watermark by more than this are not read again.
pub const OVERLAP: ChronoDuration = ChronoDuration::minutes(2);
/// Most pages one catch-up reads.
pub const MAX_PAGES: usize = 50;
/// Furthest back a catch-up reads; the 22:00 poll covers the rest.
pub const MAX_LOOKBACK: ChronoDuration = ChronoDuration::hours(24);
/// Watermark when nothing is stored: this long before now.
pub const DEFAULT_WATERMARK_AGE: ChronoDuration = ChronoDuration::minutes(15);
/// How often the watermark is written and its lag exported.
pub const FLUSH_EVERY: Duration = Duration::from_secs(10);
/// Timeout of one page request.
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Gap between page requests.
pub const PAGE_GAP: Duration = Duration::from_millis(100);

/// User agent of the live listener's HTTP requests.
pub const USER_AGENT: &str = "nexus-chronicle/nordic-live (lasse.alm@gsfleet.io)";

/// The retry schedule of one page: 3 retries with exponential backoff.
pub fn page_backoff() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(500))
        .with_max_delay(Duration::from_secs(5))
        .with_max_times(3)
}

/// The newest `publish_date` `nordic-live` has processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watermark {
    value: DateTime<Utc>,
    dirty: bool,
}

impl Watermark {
    /// Starts at `value`, to be stored by the next [`Watermark::flush`].
    pub fn new(value: DateTime<Utc>) -> Self {
        Self { value, dirty: true }
    }

    /// The stored watermark, or `now − 15 min` when no row exists.
    pub async fn load(pool: &PgPool, now: DateTime<Utc>) -> sqlx::Result<Self> {
        let stored: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT watermark FROM nordic_live_state")
                .fetch_optional(pool)
                .await?;
        Ok(match stored {
            Some(value) => Self {
                value,
                dirty: false,
            },
            None => Self::new(now - DEFAULT_WATERMARK_AGE),
        })
    }

    /// The watermark.
    pub fn value(&self) -> DateTime<Utc> {
        self.value
    }

    /// Moves the watermark forward to `published` if it is newer.
    pub fn observe(&mut self, published: Option<DateTime<Utc>>) {
        if let Some(p) = published.filter(|p| *p > self.value) {
            self.value = p;
            self.dirty = true;
        }
    }

    /// Seconds from the watermark to `now` (0 if it is in the future).
    pub fn lag_seconds(&self, now: DateTime<Utc>) -> f64 {
        ((now - self.value).num_milliseconds().max(0)) as f64 / 1000.0
    }

    fn take_dirty(&mut self) -> Option<DateTime<Utc>> {
        std::mem::take(&mut self.dirty).then_some(self.value)
    }
}

/// A watermark shared by the main loop and the catch-up task.
pub type SharedWatermark = Arc<Mutex<Watermark>>;

/// Writes the watermark if it moved since the last write and exports its lag.
/// Called every [`FLUSH_EVERY`], never per item. A failed write is logged and
/// retried on the next call.
pub async fn flush(pool: &PgPool, watermark: &SharedWatermark, now: DateTime<Utc>) {
    let (to_store, lag) = {
        let mut w = watermark.lock().expect("watermark lock is never poisoned");
        (w.take_dirty(), w.lag_seconds(now))
    };
    gauge!("nordic_live_watermark_lag_seconds").set(lag);
    let Some(value) = to_store else { return };
    let stored = sqlx::query(
        "INSERT INTO nordic_live_state (id, watermark, updated_at) VALUES (true, $1, now()) \
         ON CONFLICT (id) DO UPDATE \
         SET watermark = GREATEST(nordic_live_state.watermark, EXCLUDED.watermark), \
             updated_at = now()",
    )
    .bind(value)
    .execute(pool)
    .await;
    if let Err(e) = stored {
        warn!(error = %e, "storing the nordic-live watermark failed, retrying later");
        if let Ok(mut w) = watermark.lock() {
            w.dirty = true;
        }
    }
}

/// Sets every catch-up series to 0 so rates and alerts see them early.
pub fn describe_catchup_metrics() {
    for result in ["ok", "capped", "failed"] {
        counter!("nordic_live_catchup_runs_total", "result" => result).absolute(0);
    }
    counter!("nordic_live_catchup_items_total").absolute(0);
    counter!("nordic_live_catchup_pages_total").absolute(0);
    gauge!("nordic_live_watermark_lag_seconds").set(0.0);
}

/// What one catch-up read.
#[derive(Debug)]
pub struct Fetched {
    /// Items newer than `watermark − overlap`, oldest first.
    pub items: Vec<NewsItem>,
    /// Pages read.
    pub pages: usize,
    /// Why it stopped early: `pages` or `lookback`.
    pub capped: Option<&'static str>,
    /// The watermark it started from.
    pub watermark: DateTime<Utc>,
}

/// Reads the HTTP feed from `first_page` back to `watermark − 2 min` (and never
/// beyond 24 h or [`MAX_PAGES`] pages). A page that fails is retried per
/// `backoff`; if it still fails the whole catch-up fails and the next reconnect
/// tries again.
pub async fn fetch_catchup(
    client: &tokio::sync::Mutex<MfnClient>,
    first_page: reqwest::Url,
    watermark: DateTime<Utc>,
    now: DateTime<Utc>,
    backoff: ExponentialBuilder,
) -> anyhow::Result<Fetched> {
    let stop_before = watermark - OVERLAP;
    let floor = now - MAX_LOOKBACK;
    let keep_from = stop_before.max(floor);
    let mut items: Vec<NewsItem> = Vec::new();
    let mut pages = 0;
    let mut capped = None;
    let mut url = first_page;
    loop {
        let page_url = url.clone();
        let page =
            (|| async { fetch_feed_page(&mut *client.lock().await, page_url.clone()).await })
                .retry(backoff)
                .notify(|e: &anyhow::Error, retry_in| {
                    warn!(
                        error = format!("{e:#}"),
                        ?retry_in,
                        "catch-up page failed, retrying"
                    );
                })
                .await?;
        pages += 1;
        counter!("nordic_live_catchup_pages_total").increment(1);
        let oldest = page.items.iter().map(|i| i.published_at).min();
        items.extend(
            page.items
                .into_iter()
                .filter(|i| i.published_at >= keep_from),
        );
        let Some(next) = page.next_url else { break };
        if oldest.is_some_and(|o| o < stop_before) {
            break;
        }
        if oldest.is_some_and(|o| o < floor) {
            warn!("catch-up reached 24 h back, the 22:00 poll covers the rest");
            capped = Some("lookback");
            break;
        }
        if pages >= MAX_PAGES {
            warn!(
                pages,
                "catch-up page cap reached, the 22:00 poll covers the rest"
            );
            capped = Some("pages");
            break;
        }
        url = next;
    }
    items.sort_by_key(|i| i.published_at);
    Ok(Fetched {
        items,
        pages,
        capped,
        watermark,
    })
}

/// One catch-up run for the main loop: fetches and sends the result on `out`.
/// Failures are logged at `warn` and counted; nothing here can touch the socket.
pub async fn run_once(
    client: &tokio::sync::Mutex<MfnClient>,
    first_page: &reqwest::Url,
    watermark: &SharedWatermark,
    out: &mpsc::Sender<Fetched>,
    backoff: ExponentialBuilder,
) {
    let from = watermark
        .lock()
        .expect("watermark lock is never poisoned")
        .value();
    match fetch_catchup(client, first_page.clone(), from, Utc::now(), backoff).await {
        Ok(fetched) => {
            let result = if fetched.capped.is_some() {
                "capped"
            } else {
                "ok"
            };
            counter!("nordic_live_catchup_runs_total", "result" => result).increment(1);
            let _ = out.send(fetched).await;
        }
        Err(e) => {
            counter!("nordic_live_catchup_runs_total", "result" => "failed").increment(1);
            warn!(
                error = format!("{e:#}"),
                "catch-up failed, waiting for the next reconnect"
            );
        }
    }
}

/// Starts `run` after a (re)connect, one run at a time. Connects that arrive
/// together, or while a run is going, are merged: together they cause one run
/// (or, if they arrived during one, a single follow-up run, because the gap
/// they close may have opened after that run's first page). Returns when the
/// connection-event channel closes.
pub async fn run_coordinator<F, Fut>(mut events: mpsc::UnboundedReceiver<ConnEvent>, mut run: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    fn connected_pending(events: &mut mpsc::UnboundedReceiver<ConnEvent>) -> bool {
        let mut any = false;
        while let Ok(e) = events.try_recv() {
            any |= e == ConnEvent::Connected;
        }
        any
    }
    while let Some(event) = events.recv().await {
        if event != ConnEvent::Connected {
            continue;
        }
        connected_pending(&mut events);
        loop {
            run().await;
            if !connected_pending(&mut events) {
                break;
            }
        }
    }
}

/// The info line of a finished catch-up.
pub fn log_run(f: &Fetched, new_relevant: usize, watermark_after: DateTime<Utc>) {
    info!(
        pages = f.pages,
        items = f.items.len(),
        new_relevant,
        capped = f.capped.unwrap_or(""),
        watermark_before = %f.watermark.to_rfc3339(),
        watermark_after = %watermark_after.to_rfc3339(),
        "nordic-live catch-up"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 7, h, m, 0).unwrap()
    }

    #[test]
    fn the_watermark_only_moves_forward_and_reports_its_lag() {
        let mut w = Watermark::new(at(10, 0));
        assert!(w.take_dirty().is_some());
        assert!(w.take_dirty().is_none());
        w.observe(Some(at(9, 0)));
        assert!(w.take_dirty().is_none(), "older items do not move it");
        w.observe(None);
        w.observe(Some(at(10, 5)));
        assert_eq!(w.value(), at(10, 5));
        assert_eq!(w.take_dirty(), Some(at(10, 5)));
        assert_eq!(w.lag_seconds(at(10, 20)), 900.0);
        assert_eq!(w.lag_seconds(at(10, 0)), 0.0);
    }

    #[tokio::test]
    async fn connects_in_quick_succession_cause_one_run() {
        let (tx, rx) = mpsc::unbounded_channel();
        for _ in 0..3 {
            tx.send(ConnEvent::Connected).unwrap();
        }
        tx.send(ConnEvent::FastReconnect).unwrap();
        drop(tx);
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        run_coordinator(rx, move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_connect_during_a_run_causes_one_follow_up() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(ConnEvent::Connected).unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let (counted, sender) = (runs.clone(), tx.clone());
        let handle = tokio::spawn(run_coordinator(rx, move || {
            let (counted, sender) = (counted.clone(), sender.clone());
            async move {
                if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    sender.send(ConnEvent::Connected).unwrap();
                    sender.send(ConnEvent::Connected).unwrap();
                }
            }
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        drop(tx);
    }
}
