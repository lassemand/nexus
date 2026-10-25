//! Daily-bar sync against a mocked Saxo chart API and an ephemeral Postgres
//! (for the progress table). Skipped unless `TEST_DATABASE_URL` is set.

mod common;

use alpha::saxo::{ChartClient, SaxoToken};
use chronicle_lib::daily_bars::{
    backfill_floor, load_state, sync_instrument, BarSink, Instrument, State,
};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use common::TestDb;
use model::generated::DailyBar;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct RecordingSink {
    bars: Mutex<Vec<DailyBar>>,
    /// Fail the n-th publish (1-based), if set.
    fail_on: Option<usize>,
}

impl RecordingSink {
    fn new() -> Self {
        Self {
            bars: Mutex::new(vec![]),
            fail_on: None,
        }
    }
    fn dates(&self) -> Vec<String> {
        self.bars
            .lock()
            .unwrap()
            .iter()
            .map(|b| b.date.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl BarSink for RecordingSink {
    async fn publish(&self, bar: &DailyBar) -> Result<(), String> {
        let mut bars = self.bars.lock().unwrap();
        if self.fail_on == Some(bars.len() + 1) {
            return Err("broker unavailable".into());
        }
        bars.push(bar.clone());
        Ok(())
    }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 28, 16, 0, 0).unwrap()
}

fn d(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

fn instrument() -> Instrument {
    Instrument {
        ticker: "GOMX".into(),
        exchange_mic: "FNSE".into(),
        uic: 4711,
        currency: "SEK".into(),
    }
}

fn client(server: &MockServer) -> ChartClient {
    let token = Arc::new(Mutex::new(SaxoToken {
        access_token: "stream-token".into(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
    }));
    ChartClient::new(reqwest::Client::new(), server.uri(), token, Duration::ZERO)
}

/// A chart response with one sample per `(date, close)` at UTC midnight.
fn page(bars: &[(&str, &str)]) -> ResponseTemplate {
    let data: Vec<String> = bars
        .iter()
        .map(|(date, close)| {
            format!(
                r#"{{"Time":"{date}T00:00:00.000000Z","Open":{close},"High":{close},"Low":{close},"Close":{close},"Volume":100}}"#
            )
        })
        .collect();
    ResponseTemplate::new(200).set_body_string(format!(r#"{{"Data":[{}]}}"#, data.join(",")))
}

async fn mount_page(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/chart/v1/charts"))
        .respond_with(response)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

async fn requests(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap()
}

#[tokio::test]
async fn backfill_pages_back_until_the_data_runs_out_and_never_fills_gaps() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    mount_page(
        &server,
        page(&[
            ("2026-08-26", "12.345678"),
            ("2026-08-27", "12.5"),
            ("2026-08-28", "12.6"),
        ]),
    )
    .await;
    // The 25th is missing (no trades): it must stay missing.
    mount_page(
        &server,
        page(&[
            ("2026-08-20", "11.0"),
            ("2026-08-21", "11.1"),
            ("2026-08-24", "11.2"),
        ]),
    )
    .await;
    mount_page(&server, page(&[])).await;

    let sink = RecordingSink::new();
    let out = sync_instrument(&db.pool, &mut client(&server), &sink, &instrument(), now())
        .await
        .unwrap();
    assert_eq!((out.published, out.requests), (6, 3));
    assert_eq!(
        sink.dates(),
        vec![
            "2026-08-26",
            "2026-08-27",
            "2026-08-28",
            "2026-08-20",
            "2026-08-21",
            "2026-08-24"
        ]
    );
    // Decimal strings, exactly as Saxo sent them.
    let first = sink.bars.lock().unwrap()[0].clone();
    assert_eq!(
        (
            first.close.as_str(),
            first.currency.as_str(),
            first.source.as_str()
        ),
        ("12.345678", "SEK", "saxo")
    );
    assert_eq!(
        (
            first.ticker.as_str(),
            first.exchange_mic.as_str(),
            first.volume
        ),
        ("GOMX", "FNSE", 100)
    );

    // The second request continues one second before the oldest bar of the first page.
    let reqs = requests(&server).await;
    let time: Vec<_> = reqs[1]
        .url
        .query_pairs()
        .filter(|(k, _)| k == "Time")
        .map(|(_, v)| v.to_string())
        .collect();
    assert_eq!(time, vec!["2026-08-25T23:59:59.000000Z"]);

    let state = load_state(&db.pool, "GOMX", "FNSE").await.unwrap().unwrap();
    assert_eq!(
        state,
        State {
            backfilled_from: Some(backfill_floor()),
            last_published_date: Some(d("2026-08-28"))
        }
    );
    db.drop_db().await;
}

#[tokio::test]
async fn backfill_stops_at_the_floor_without_another_request() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    mount_page(
        &server,
        page(&[
            ("2014-12-31", "9.0"),
            ("2015-01-02", "10.0"),
            ("2015-01-05", "10.5"),
        ]),
    )
    .await;

    let sink = RecordingSink::new();
    let out = sync_instrument(&db.pool, &mut client(&server), &sink, &instrument(), now())
        .await
        .unwrap();
    assert_eq!((out.published, out.requests), (2, 1));
    assert_eq!(sink.dates(), vec!["2015-01-02", "2015-01-05"]); // 2014 is before the floor
    let state = load_state(&db.pool, "GOMX", "FNSE").await.unwrap().unwrap();
    assert_eq!(state.backfilled_from, Some(backfill_floor()));
    db.drop_db().await;
}

#[tokio::test]
async fn incremental_run_refetches_ten_weekdays_before_the_last_published_date() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    sqlx::query(
        "INSERT INTO saxo_daily_bars_state (ticker, exchange_mic, backfilled_from, last_published_date) \
         VALUES ('GOMX','FNSE','2015-01-01','2026-08-20')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let server = MockServer::start().await;
    // 10 weekdays before 2026-08-20 is 2026-08-06; the 3rd is older and is not republished.
    mount_page(
        &server,
        page(&[
            ("2026-08-03", "1"),
            ("2026-08-06", "2"),
            ("2026-08-21", "3"),
            ("2026-08-28", "4"),
        ]),
    )
    .await;

    let sink = RecordingSink::new();
    let out = sync_instrument(&db.pool, &mut client(&server), &sink, &instrument(), now())
        .await
        .unwrap();
    assert_eq!((out.published, out.requests), (3, 1));
    assert_eq!(sink.dates(), vec!["2026-08-06", "2026-08-21", "2026-08-28"]);
    let q: Vec<(String, String)> = requests(&server).await[0]
        .url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert!(q.contains(&("Count".into(), "21".into())), "{q:?}"); // 16 weekdays + 5
    assert!(q.contains(&("Mode".into(), "UpTo".into())));
    let state = load_state(&db.pool, "GOMX", "FNSE").await.unwrap().unwrap();
    assert_eq!(state.last_published_date, Some(d("2026-08-28")));
    assert_eq!(state.backfilled_from, Some(backfill_floor()));
    db.drop_db().await;
}

#[tokio::test]
async fn progress_is_not_recorded_for_a_page_the_broker_did_not_acknowledge() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    mount_page(
        &server,
        page(&[
            ("2026-08-26", "1"),
            ("2026-08-27", "2"),
            ("2026-08-28", "3"),
        ]),
    )
    .await;
    let sink = RecordingSink {
        bars: Mutex::new(vec![]),
        fail_on: Some(2),
    };
    let err = sync_instrument(&db.pool, &mut client(&server), &sink, &instrument(), now())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("publishing GOMX"), "{err}");
    assert!(load_state(&db.pool, "GOMX", "FNSE")
        .await
        .unwrap()
        .is_none());
    db.drop_db().await;
}

#[tokio::test]
async fn an_unfinished_backfill_resumes_before_what_is_already_published() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    sqlx::query(
        "INSERT INTO saxo_daily_bars_state (ticker, exchange_mic, backfilled_from, last_published_date) \
         VALUES ('GOMX','FNSE','2026-08-10','2026-08-28')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let server = MockServer::start().await;
    mount_page(&server, page(&[("2026-08-10", "5"), ("2026-08-28", "6")])).await; // recent pass
    mount_page(&server, page(&[("2026-08-03", "4"), ("2026-08-07", "4.5")])).await; // backfill page
    mount_page(&server, page(&[])).await; // history exhausted

    let sink = RecordingSink::new();
    sync_instrument(&db.pool, &mut client(&server), &sink, &instrument(), now())
        .await
        .unwrap();
    let reqs = requests(&server).await;
    assert_eq!(reqs.len(), 3);
    let time = |i: usize| {
        reqs[i]
            .url
            .query_pairs()
            .find(|(k, _)| k == "Time")
            .unwrap()
            .1
            .to_string()
    };
    assert_eq!(time(1), "2026-08-10T00:00:00.000000Z"); // resumes at the oldest published day
    let state = load_state(&db.pool, "GOMX", "FNSE").await.unwrap().unwrap();
    assert_eq!(state.backfilled_from, Some(backfill_floor()));
    db.drop_db().await;
}

/// The fetch path only reads the stream's token: nothing here can reach a token
/// endpoint, and the mock proves none was called.
#[tokio::test]
async fn the_sync_never_calls_a_token_endpoint() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("grant_type", "refresh_token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    mount_page(&server, page(&[("2026-08-28", "1")])).await;
    mount_page(&server, page(&[])).await;
    let sink = RecordingSink::new();
    sync_instrument(&db.pool, &mut client(&server), &sink, &instrument(), now())
        .await
        .unwrap();
    assert!(requests(&server)
        .await
        .iter()
        .all(|r| r.method.as_str() == "GET"));
    assert!(requests(&server)
        .await
        .iter()
        .all(|r| r.url.path() == "/chart/v1/charts"));
    db.drop_db().await;
}
