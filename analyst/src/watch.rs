//! The watch list: which instruments `saxo-stream` streams quotes for.
//!
//! Scheduled part: every active report target whose scheduled date is today or
//! later in its exchange's time zone and whose instant is at most `WATCH_LEAD`
//! ahead. Pinned part: an instrument with a trade in `pending_entry` or `open`
//! is held by a [`crate::quote_book::Pin`] from the trade's task, so it stays
//! until the trade ends. The scheduled part is recomputed every scheduler tick
//! and right after every scheduler pass (which is when targets change).

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use model::session::Sessions;
use tokio::sync::Notify;
use tracing::warn;

use crate::calendar::SessionsCache;
use crate::clock::Clock;
use crate::quote_book::{key, Pair, QuoteBook};
use crate::store::{Store, Target};

/// The scheduled instruments at `now`: targets dated today or later on their
/// exchange's calendar, due within `lead`. A target on an exchange the calendar
/// does not know is left out: its trade would be `calendar_unknown` and take no
/// quote.
pub fn scheduled_pairs(
    targets: &[Target],
    sessions: &Sessions,
    now: DateTime<Utc>,
    lead: Duration,
) -> BTreeSet<Pair> {
    let until = now + ChronoDuration::from_std(lead).unwrap_or(ChronoDuration::MAX);
    targets
        .iter()
        .filter(|target| {
            sessions
                .local_date(&target.exchange_mic, now)
                .is_some_and(|today| target.expected_date >= today)
                && target.expected_at <= until
        })
        .map(|target| key(&target.ticker, &target.exchange_mic))
        .collect()
}

/// Where the active report targets are read from.
#[async_trait]
pub trait TargetSource: Send + Sync {
    /// Targets that can still change.
    async fn active_targets(&self) -> Result<Vec<Target>, sqlx::Error>;
}

#[async_trait]
impl TargetSource for Store {
    async fn active_targets(&self) -> Result<Vec<Target>, sqlx::Error> {
        Store::active_targets(self).await
    }
}

/// The timer refresh is this many scheduler ticks apart; the scheduler's
/// notification is the primary trigger.
pub const SAFETY_NET_TICKS: u32 = 5;

/// What the updater reads and writes.
pub struct WatchUpdater {
    /// The targets.
    pub targets: Arc<dyn TargetSource>,
    /// The calendars.
    pub sessions: SessionsCache,
    /// The book whose scheduled list is set.
    pub book: Arc<QuoteBook>,
    /// The clock.
    pub clock: Arc<dyn Clock>,
    /// `WATCH_LEAD`.
    pub lead: Duration,
}

impl WatchUpdater {
    /// Recomputes the scheduled list once. A failed read keeps the last list.
    pub async fn refresh(&self) {
        match self.targets.active_targets().await {
            Ok(targets) => {
                let sessions = self.sessions.get();
                let list = scheduled_pairs(&targets, &sessions, self.clock.now(), self.lead);
                self.book.set_scheduled(list);
            }
            Err(error) => warn!(%error, "reading the report targets for the watch list failed"),
        }
    }

    /// Refreshes whenever `changed` is notified (the scheduler notifies after
    /// every pass, which is when targets change) and, as a slow safety net,
    /// every [`SAFETY_NET_TICKS`] times `tick`, until `shutdown` completes.
    pub async fn run(
        &self,
        tick: Duration,
        changed: Arc<Notify>,
        shutdown: impl Future<Output = ()>,
    ) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            self.refresh().await;
            tokio::select! {
                () = &mut shutdown => return,
                () = changed.notified() => {}
                () = tokio::time::sleep(tick * SAFETY_NET_TICKS) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::{NaiveDate, NaiveTime, TimeZone};
    use model::session::{Calendar, Exchange};

    use super::*;
    use crate::clock::ManualClock;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn sessions() -> Sessions {
        let market = |mic: &str, tz: chrono_tz::Tz, open, close| {
            (
                Exchange {
                    mic: mic.into(),
                    country: "XX".into(),
                    timezone: tz,
                    open,
                    close,
                    half_day_close: None,
                },
                Calendar::from_holidays(
                    [d(2026, 1, 1), d(2026, 11, 16), d(2026, 12, 25)]
                        .into_iter()
                        .collect(),
                    [d(2026, 12, 24)].into_iter().collect(),
                )
                .expect("calendar"),
            )
        };
        Sessions::new(vec![
            market("XSTO", chrono_tz::Europe::Stockholm, t(9, 0), t(17, 30)),
            market("XHEL", chrono_tz::Europe::Helsinki, t(10, 0), t(18, 30)),
        ])
    }

    fn target(ticker: &str, mic: &str, date: (i32, u32, u32), at: DateTime<Utc>) -> Target {
        Target {
            id: 1,
            ticker: ticker.into(),
            exchange_mic: mic.into(),
            report_kind: "interim".into(),
            fiscal_period: None,
            expected_date: NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap(),
            expected_time_known: true,
            expected_at: at,
            signal_event_id: 1,
            status: "ready".into(),
            prep_due_at: at,
            prep_attempts: 0,
            retry_after: None,
            prepared_for_at: None,
            created_at: at,
            updated_at: at,
        }
    }

    fn utc(d: u32, h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap()
    }

    const LEAD: Duration = Duration::from_secs(12 * 3600);

    fn pairs(list: &[(&str, &str)]) -> BTreeSet<Pair> {
        list.iter().map(|(t, m)| key(t, m)).collect()
    }

    #[test]
    fn a_report_tomorrow_at_0730_is_watched_from_1930_with_a_12_hour_lead() {
        let tomorrow = target("AAA", "XSTO", (2026, 11, 11), utc(11, 6, 30));
        let sessions = sessions();
        assert!(scheduled_pairs(
            std::slice::from_ref(&tomorrow),
            &sessions,
            utc(10, 18, 29),
            LEAD
        )
        .is_empty());
        assert_eq!(
            scheduled_pairs(
                std::slice::from_ref(&tomorrow),
                &sessions,
                utc(10, 18, 30),
                LEAD
            ),
            pairs(&[("AAA", "XSTO")])
        );
        assert_eq!(
            scheduled_pairs(
                std::slice::from_ref(&tomorrow),
                &sessions,
                utc(10, 21, 0),
                LEAD
            ),
            pairs(&[("AAA", "XSTO")])
        );
    }

    #[test]
    fn a_report_earlier_today_that_has_not_come_stays_watched_but_yesterdays_does_not() {
        let sessions = sessions();
        let earlier_today = target("AAA", "XSTO", (2026, 11, 10), utc(10, 6, 30));
        let yesterday = target("BBB", "XSTO", (2026, 11, 9), utc(9, 6, 30));
        let now = utc(10, 12, 0);
        assert_eq!(
            scheduled_pairs(&[earlier_today, yesterday], &sessions, now, LEAD),
            pairs(&[("AAA", "XSTO")])
        );
    }

    #[test]
    fn today_is_the_exchanges_today_not_utc() {
        let sessions = sessions();
        let helsinki_today = target("AAA", "XHEL", (2026, 11, 11), utc(10, 22, 30));
        assert_eq!(
            scheduled_pairs(
                std::slice::from_ref(&helsinki_today),
                &sessions,
                utc(10, 22, 45),
                LEAD
            ),
            pairs(&[("AAA", "XHEL")]),
            "00:45 on the 11th in Helsinki"
        );
        let stockholm_yesterday = target("BBB", "XSTO", (2026, 11, 10), utc(10, 12, 0));
        assert!(
            scheduled_pairs(&[stockholm_yesterday], &sessions, utc(10, 23, 30), LEAD).is_empty(),
            "00:30 on the 11th in Stockholm"
        );
    }

    #[test]
    fn an_exchange_the_calendar_does_not_know_is_left_out() {
        let unknown = target("AAA", "XZZZ", (2026, 11, 10), utc(10, 12, 0));
        assert!(scheduled_pairs(&[unknown], &sessions(), utc(10, 8, 0), LEAD).is_empty());
    }

    struct Fixed(Mutex<Result<Vec<Target>, ()>>);

    #[async_trait]
    impl TargetSource for Fixed {
        async fn active_targets(&self) -> Result<Vec<Target>, sqlx::Error> {
            self.0
                .lock()
                .unwrap()
                .clone()
                .map_err(|()| sqlx::Error::PoolTimedOut)
        }
    }

    fn updater(source: Arc<Fixed>, now: DateTime<Utc>) -> (Arc<QuoteBook>, WatchUpdater) {
        let clock = Arc::new(ManualClock::new(now));
        let book = QuoteBook::new(clock.clone(), Duration::from_secs(3));
        let updater = WatchUpdater {
            targets: source,
            sessions: SessionsCache::new(sessions()),
            book: book.clone(),
            clock,
            lead: LEAD,
        };
        (book, updater)
    }

    #[tokio::test]
    async fn the_updater_sets_the_scheduled_list_and_keeps_it_when_a_read_fails() {
        let source = Arc::new(Fixed(Mutex::new(Ok(vec![target(
            "AAA",
            "XSTO",
            (2026, 11, 10),
            utc(10, 12, 0),
        )]))));
        let (book, updater) = updater(source.clone(), utc(10, 8, 0));
        updater.refresh().await;
        assert!(book.is_watched(&key("AAA", "XSTO")));
        *source.0.lock().unwrap() = Err(());
        updater.refresh().await;
        assert!(book.is_watched(&key("AAA", "XSTO")));
        *source.0.lock().unwrap() = Ok(vec![]);
        updater.refresh().await;
        assert!(!book.is_watched(&key("AAA", "XSTO")));
    }

    #[tokio::test]
    async fn a_decided_target_leaves_the_list_once_its_trade_is_over() {
        let source = Arc::new(Fixed(Mutex::new(Ok(vec![target(
            "AAA",
            "XSTO",
            (2026, 11, 11),
            utc(11, 6, 30),
        )]))));
        let (book, updater) = updater(source.clone(), utc(10, 19, 30));
        updater.refresh().await;
        assert!(book.is_watched(&key("AAA", "XSTO")));
        let pin = book.pin(key("AAA", "XSTO"));
        *source.0.lock().unwrap() = Ok(vec![]);
        updater.refresh().await;
        assert!(
            book.is_watched(&key("AAA", "XSTO")),
            "the open trade pins it"
        );
        drop(pin);
        assert!(!book.is_watched(&key("AAA", "XSTO")));
    }

    #[tokio::test]
    async fn the_updater_refreshes_when_notified_and_stops_on_shutdown() {
        let source = Arc::new(Fixed(Mutex::new(Ok(vec![]))));
        let (book, updater) = updater(source.clone(), utc(10, 8, 0));
        let updater = Arc::new(updater);
        let changed = Arc::new(Notify::new());
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = {
            let (updater, changed) = (updater.clone(), changed.clone());
            tokio::spawn(async move {
                updater
                    .run(Duration::from_secs(3600), changed, async {
                        let _ = stop_rx.await;
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        *source.0.lock().unwrap() = Ok(vec![target("AAA", "XSTO", (2026, 11, 10), utc(10, 12, 0))]);
        changed.notify_one();
        for _ in 0..200 {
            if book.is_watched(&key("AAA", "XSTO")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(book.is_watched(&key("AAA", "XSTO")));
        stop_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("stops")
            .unwrap();
    }
}
