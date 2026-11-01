//! Slack posting for `nordic-live` (NEX-222): the session label from signal's
//! exchange data, the delivery flow (claim, post, mark, English upgrade) and the
//! worker that keeps all of it off the WebSocket loop.

use super::classify::{strip_issuer_prefix, ReportKind};
use super::live::LiveItem;
use super::slack::{deliver_notice, notice_time_zone, Poster, ReportNotice};
pub use super::slack::{Outcome, SlackApi};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use metrics::counter;
use model::session::{release_session, Calendar, Exchange, Session};
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Capacity of the queue between the WebSocket loop and the Slack worker.
pub const QUEUE_CAPACITY: usize = 256;
const LOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// The label Slack messages use for a session.
pub fn session_label(s: Session) -> &'static str {
    match s {
        Session::PreOpen => "pre-open",
        Session::Intraday => "intraday",
        Session::AfterClose => "after close",
        Session::NonTradingDay => "non-trading day",
    }
}

/// Exchanges and holiday calendars from signal's database, for session labels.
#[derive(Debug, Default, Clone)]
pub struct Sessions {
    markets: HashMap<String, (Exchange, Calendar)>,
}

impl Sessions {
    /// Builds the lookup from loaded markets.
    pub fn new(markets: Vec<(Exchange, Calendar)>) -> Self {
        Self {
            markets: markets
                .into_iter()
                .map(|(e, c)| (e.mic.clone(), (e, c)))
                .collect(),
        }
    }

    /// Loads every exchange and its country's holidays (read-only queries).
    /// A country without holiday rows leaves its exchanges out: no calendar,
    /// no session.
    pub async fn load(pool: &PgPool) -> sqlx::Result<Self> {
        let load = async {
            let exchanges = sqlx::query(
                "SELECT mic, country, timezone, open_local, close_local, half_day_close_local \
                 FROM exchanges",
            )
            .fetch_all(pool)
            .await?;
            let holidays = sqlx::query("SELECT country, date, status FROM trading_holidays")
                .fetch_all(pool)
                .await?;
            Ok::<_, sqlx::Error>((exchanges, holidays))
        };
        let (exchanges, holidays) = match tokio::time::timeout(LOAD_TIMEOUT, load).await {
            Ok(r) => r?,
            Err(_) => return Err(sqlx::Error::PoolTimedOut),
        };
        let mut by_country: HashMap<String, (HashSet<NaiveDate>, HashSet<NaiveDate>)> =
            HashMap::new();
        for r in holidays {
            let entry = by_country.entry(r.get("country")).or_default();
            let date: NaiveDate = r.get("date");
            if r.get::<String, _>("status") == "closed" {
                entry.0.insert(date);
            } else {
                entry.1.insert(date);
            }
        }
        let mut markets = Vec::new();
        for r in exchanges {
            let mic: String = r.get("mic");
            let tz: String = r.get("timezone");
            let Ok(timezone) = tz.parse::<Tz>() else {
                warn!(mic = %mic, timezone = %tz, "exchanges.timezone is not an IANA time zone");
                continue;
            };
            let country: String = r.get("country");
            let Some((closed, half)) = by_country.get(&country).cloned() else {
                continue;
            };
            let Some(calendar) = Calendar::from_holidays(closed, half) else {
                continue;
            };
            let exchange = Exchange {
                mic,
                country,
                timezone,
                open: r.get::<NaiveTime, _>("open_local"),
                close: r.get::<NaiveTime, _>("close_local"),
                half_day_close: r.get("half_day_close_local"),
            };
            markets.push((exchange, calendar));
        }
        Ok(Self::new(markets))
    }

    /// The session label of a release at `at` on `mic`; `None` when the exchange
    /// or the year is unknown (never guessed).
    pub fn label(&self, mic: &str, at: DateTime<Utc>) -> Option<&'static str> {
        let (exchange, calendar) = self.markets.get(mic)?;
        release_session(at, calendar, exchange).map(session_label)
    }

    /// Number of exchanges with a usable calendar.
    pub fn len(&self) -> usize {
        self.markets.len()
    }

    /// True if no exchange has a usable calendar.
    pub fn is_empty(&self) -> bool {
        self.markets.is_empty()
    }
}

/// One report to deliver.
#[derive(Debug, Clone)]
pub struct SlackJob {
    pub item: LiveItem,
    /// Computed when the item arrived; `None` when unknown.
    pub session: Option<&'static str>,
}

/// The notice for a job, or `None` for an item that is not a results release.
pub fn build_notice(job: &SlackJob) -> Option<ReportNotice> {
    let item = &job.item;
    let report_kind = match item.report_kind? {
        "interim" => ReportKind::Interim,
        "annual" => ReportKind::Annual,
        _ => return None,
    };
    let time_zone = notice_time_zone(&item.exchange_mic);
    Some(ReportNotice {
        company_name: item.display_name(),
        ticker: item.ticker.clone(),
        exchange_mic: item.exchange_mic.clone(),
        report_kind,
        fiscal_period: item.fiscal_period.clone(),
        published_at: item.publish_date,
        time_zone,
        session: job.session.map(str::to_string),
        headline: strip_issuer_prefix(&item.title).to_string(),
        mfn_url: item.url.clone(),
        pdf_url: item.attachment_urls.first().cloned(),
        late: false,
        news_id: item.news_id,
        lang: item.lang.clone(),
    })
}

/// Claims, posts and records one report (or upgrades a posted twin to English).
pub async fn deliver(pool: &PgPool, api: &dyn SlackApi, job: &SlackJob) -> Outcome {
    match build_notice(job) {
        Some(notice) => deliver_notice(pool, api, &notice, Poster::Live).await,
        None => Outcome::SkippedAlreadyPosted,
    }
}

fn count(result: &'static str) {
    counter!("nordic_slack_posts_total", "result" => result).increment(1);
}

/// Creates the `nordic_slack_posts_total` series at zero so alert rules see them.
pub fn describe_metrics() {
    for result in ["posted", "updated", "failed", "dropped"] {
        counter!("nordic_slack_posts_total", "result" => result).absolute(0);
    }
}

/// Delivers jobs from `rx` until the channel closes. Logs one `slack_post` event per job.
pub async fn run_worker(pool: PgPool, api: Box<dyn SlackApi>, mut rx: mpsc::Receiver<SlackJob>) {
    while let Some(job) = rx.recv().await {
        let outcome = deliver(&pool, api.as_ref(), &job).await;
        match outcome {
            Outcome::Posted => count("posted"),
            Outcome::Updated => count("updated"),
            Outcome::Failed => count("failed"),
            Outcome::SkippedAlreadyPosted | Outcome::InFlight => {}
        }
        info!(
            event = "slack_post",
            slack = outcome.as_str(),
            news_id = %job.item.news_id,
            ticker = %job.item.ticker,
            exchange_mic = %job.item.exchange_mic,
            "nordic-live slack"
        );
    }
}

/// The jobs for a message's dispositions: new results releases only (not
/// company news, corrections or items the poll already ingested).
pub fn jobs_for(dispositions: Vec<super::live::Disposition>, sessions: &Sessions) -> Vec<SlackJob> {
    dispositions
        .into_iter()
        .filter_map(|d| match d {
            super::live::Disposition::New(item) if item.is_slack_candidate() => {
                let session = sessions.label(&item.exchange_mic, item.publish_date);
                Some(SlackJob {
                    item: *item,
                    session,
                })
            }
            _ => None,
        })
        .collect()
}

/// Queues a job without ever waiting. A full queue drops the post (the poll
/// posts it later, marked late). Returns whether it was queued.
pub fn enqueue(tx: &mpsc::Sender<SlackJob>, job: SlackJob) -> bool {
    match tx.try_send(job) {
        Ok(()) => true,
        Err(e) => {
            let job = match e {
                mpsc::error::TrySendError::Full(j) | mpsc::error::TrySendError::Closed(j) => j,
            };
            count("dropped");
            error!(
                news_id = %job.item.news_id,
                ticker = %job.item.ticker,
                "slack queue full or closed, post dropped"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nordic::live::Disposition;
    use crate::nordic::slack::{PostedRef, SlackError, SlackMessage};
    use async_trait::async_trait;
    use chrono::{NaiveTime, TimeZone};
    use std::sync::Mutex;
    use uuid::Uuid;

    #[derive(Default)]
    struct FakeSlack {
        fail: bool,
        calls: Mutex<Vec<String>>,
    }

    impl FakeSlack {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SlackApi for FakeSlack {
        async fn post(&self, msg: &SlackMessage) -> Result<PostedRef, SlackError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("post: {}", msg.text));
            if self.fail {
                return Err(SlackError::Status(500));
            }
            Ok(PostedRef {
                channel: "C1".into(),
                ts: "1.1".into(),
            })
        }

        async fn update(
            &self,
            at: &PostedRef,
            msg: &SlackMessage,
        ) -> Result<PostedRef, SlackError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("update {}: {}", at.ts, msg.text));
            Ok(at.clone())
        }
    }

    fn item(id: u128, lang: &str, report_kind: Option<&'static str>) -> LiveItem {
        LiveItem {
            news_id: Uuid::from_u128(id),
            ticker: "FFARMS".into(),
            exchange_mic: "XCSE".into(),
            mfn_slug: "firstfarms".into(),
            publish_date: Utc.with_ymd_and_hms(2026, 11, 24, 6, 30, 0).unwrap(),
            received_at: Utc.with_ymd_and_hms(2026, 11, 24, 6, 30, 2).unwrap(),
            lang: Some(lang.into()),
            tags: vec![],
            source: None,
            report_kind,
            fiscal_period: Some("2026-Q3".into()),
            is_correction: false,
            calendar_update: false,
            company_name: Some("FirstFarms A/S (FFARMS)".into()),
            author_name: None,
            title: "FirstFarms A/S: Quarterly update 1 January – 30 September 2026".into(),
            url: Some("https://mfn.se/a/x".into()),
            attachment_urls: vec!["https://x.test/file.pdf".into()],
        }
    }

    fn job(id: u128, lang: &str) -> SlackJob {
        SlackJob {
            item: item(id, lang, Some("interim")),
            session: Some("pre-open"),
        }
    }

    const MESSAGE: &str = "📊 *FirstFarms A/S (FFARMS)* · Q3 2026 interim\n\
         24 Nov 2026 07:30 CET · pre-open\n\
         Quarterly update 1 January – 30 September 2026\n\
         <https://mfn.se/a/x|MFN> · <https://x.test/file.pdf|PDF>";

    #[sqlx::test(migrations = "./migrations")]
    async fn a_report_is_posted_once_with_the_expected_text(pool: PgPool) {
        let api = FakeSlack::default();
        assert_eq!(deliver(&pool, &api, &job(1, "en")).await, Outcome::Posted);
        assert_eq!(api.calls(), vec![format!("post: {MESSAGE}")]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_same_item_twice_posts_once(pool: PgPool) {
        let api = FakeSlack::default();
        assert_eq!(deliver(&pool, &api, &job(1, "en")).await, Outcome::Posted);
        assert_eq!(
            deliver(&pool, &api, &job(1, "en")).await,
            Outcome::SkippedAlreadyPosted
        );
        assert_eq!(api.calls().len(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn swedish_then_english_posts_then_updates(pool: PgPool) {
        let api = FakeSlack::default();
        assert_eq!(deliver(&pool, &api, &job(1, "sv")).await, Outcome::Posted);
        assert_eq!(deliver(&pool, &api, &job(2, "en")).await, Outcome::Updated);
        let calls = api.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].starts_with("update 1.1: "));
        // The English twin is now the stored one: a repeat changes nothing.
        assert_eq!(
            deliver(&pool, &api, &job(2, "en")).await,
            Outcome::SkippedAlreadyPosted
        );
        assert_eq!(api.calls().len(), 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn english_then_swedish_posts_once_without_update(pool: PgPool) {
        let api = FakeSlack::default();
        assert_eq!(deliver(&pool, &api, &job(1, "en")).await, Outcome::Posted);
        assert_eq!(
            deliver(&pool, &api, &job(2, "sv")).await,
            Outcome::SkippedAlreadyPosted
        );
        assert_eq!(api.calls().len(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_failing_slack_marks_the_row_failed_and_the_worker_goes_on(pool: PgPool) {
        let failing = FakeSlack {
            fail: true,
            ..Default::default()
        };
        assert_eq!(
            deliver(&pool, &failing, &job(1, "en")).await,
            Outcome::Failed
        );
        let (status, error): (String, Option<String>) =
            sqlx::query_as("SELECT status, last_error FROM slack_report_posts")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "failed");
        assert!(error.unwrap().contains("500"));

        let (tx, rx) = mpsc::channel(8);
        let ok = std::sync::Arc::new(FakeSlack::default());
        struct Shared(std::sync::Arc<FakeSlack>);
        #[async_trait]
        impl SlackApi for Shared {
            async fn post(&self, m: &SlackMessage) -> Result<PostedRef, SlackError> {
                self.0.post(m).await
            }
            async fn update(
                &self,
                a: &PostedRef,
                m: &SlackMessage,
            ) -> Result<PostedRef, SlackError> {
                self.0.update(a, m).await
            }
        }
        let worker = tokio::spawn(run_worker(pool.clone(), Box::new(Shared(ok.clone())), rx));
        let mut other = job(3, "en");
        other.item.publish_date = Utc.with_ymd_and_hms(2026, 11, 25, 6, 30, 0).unwrap();
        assert!(enqueue(&tx, other));
        drop(tx);
        worker.await.unwrap();
        assert_eq!(ok.calls().len(), 1);
    }

    #[test]
    fn a_full_queue_drops_instead_of_blocking() {
        let (tx, _rx) = mpsc::channel(1);
        assert!(enqueue(&tx, job(1, "en")));
        assert!(!enqueue(&tx, job(2, "en")));
    }

    #[test]
    fn only_new_results_releases_become_jobs() {
        let sessions = Sessions::default();
        let correction = LiveItem {
            is_correction: true,
            ..item(3, "en", Some("interim"))
        };
        let ds = vec![
            Disposition::New(Box::new(item(1, "en", Some("interim")))),
            Disposition::New(Box::new(item(2, "en", None))),
            Disposition::New(Box::new(correction)),
            Disposition::AlreadyIngested(Uuid::from_u128(4)),
        ];
        let jobs = jobs_for(ds, &sessions);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].item.news_id, Uuid::from_u128(1));
        assert_eq!(jobs[0].session, None);
    }

    #[test]
    fn display_name_drops_the_ticker_suffix_and_falls_back() {
        let mut i = item(1, "en", Some("interim"));
        assert_eq!(i.display_name(), "FirstFarms A/S");
        i.company_name = None;
        i.author_name = Some("FirstFarms (MFN)".into());
        assert_eq!(i.display_name(), "FirstFarms (MFN)");
        i.author_name = None;
        assert_eq!(i.display_name(), "FFARMS");
    }

    fn market(mic: &str, tz: Tz, close: (u32, u32), closed: &[NaiveDate]) -> (Exchange, Calendar) {
        (
            Exchange {
                mic: mic.into(),
                country: "XX".into(),
                timezone: tz,
                open: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                close: NaiveTime::from_hms_opt(close.0, close.1, 0).unwrap(),
                half_day_close: None,
            },
            Calendar::from_holidays(
                closed
                    .iter()
                    .copied()
                    .chain([NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()])
                    .collect(),
                Default::default(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn session_labels_per_exchange() {
        let holiday = NaiveDate::from_ymd_opt(2026, 12, 25).unwrap();
        let s = Sessions::new(vec![
            market("FNSE", chrono_tz::Europe::Stockholm, (17, 30), &[holiday]),
            market("XCSE", chrono_tz::Europe::Copenhagen, (17, 0), &[]),
        ]);
        let utc = |d: u32, h: u32, m: u32, month: u32| {
            Utc.with_ymd_and_hms(2026, month, d, h, m, 0).unwrap()
        };
        assert_eq!(s.label("FNSE", utc(24, 6, 30, 11)), Some("pre-open"));
        assert_eq!(s.label("XCSE", utc(24, 16, 15, 11)), Some("after close"));
        assert_eq!(s.label("FNSE", utc(25, 9, 0, 12)), Some("non-trading day"));
        assert_eq!(s.label("XSTO", utc(24, 6, 30, 11)), None);
        assert_eq!(
            s.label("FNSE", utc(1, 9, 0, 6) + chrono::Duration::days(366)),
            None
        );
        assert_eq!(Sessions::default().label("FNSE", utc(24, 6, 30, 11)), None);
    }

    #[sqlx::test]
    async fn sessions_load_from_signals_tables(pool: PgPool) {
        sqlx::query("CREATE TABLE exchanges (mic TEXT PRIMARY KEY, country TEXT NOT NULL, timezone TEXT NOT NULL, open_local TIME NOT NULL, close_local TIME NOT NULL, half_day_close_local TIME NULL, currency TEXT NOT NULL)")
            .execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE trading_holidays (country TEXT NOT NULL, date DATE NOT NULL, status TEXT NOT NULL, PRIMARY KEY (country, date))")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO exchanges VALUES ('FNSE','SE','Europe/Stockholm','09:00','17:30','13:00','SEK'), ('XCSE','DK','Europe/Copenhagen','09:00','17:00',NULL,'DKK'), ('XBAD','NO','Mars/Base','09:00','17:00',NULL,'NOK')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO trading_holidays VALUES ('SE','2026-12-25','closed'), ('SE','2026-12-24','half_day'), ('DK','2026-12-25','closed')")
            .execute(&pool).await.unwrap();
        let s = Sessions::load(&pool).await.unwrap();
        assert_eq!(s.len(), 2);
        let at = |d: u32, h: u32, m: u32| Utc.with_ymd_and_hms(2026, 12, d, h, m, 0).unwrap();
        assert_eq!(s.label("FNSE", at(25, 9, 0)), Some("non-trading day"));
        assert_eq!(s.label("FNSE", at(24, 12, 30)), Some("after close"));
        assert_eq!(s.label("XBAD", at(10, 9, 0)), None);
    }

    #[tokio::test]
    async fn an_unreachable_signal_database_gives_no_sessions() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(300))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();
        assert!(Sessions::load(&pool).await.is_err());
    }
}
