//! Nordic publishing against mocked MFN endpoints (NEX-160, NEX-161, NEX-171).
//! Database-backed tests are skipped unless `TEST_DATABASE_URL` is set (the
//! smoke-test workflow sets it); the rest always run.

mod common;

use async_trait::async_trait;
use chronicle_lib::nordic::calendar::{refresh_company, MfnClient, Refresh};
use chronicle_lib::nordic::db::{self, Company};
use chronicle_lib::nordic::events::EventSink;
use chronicle_lib::nordic::ingest::{process_companies, RunSummary};
use chronicle_lib::nordic::NORDIC_MICS;
use common::TestDb;
use model::generated::SpecialEvent;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NEWS_GOMSPACE: &str = include_str!("fixtures/mfn_news_gomspace.json");
const NEWS_VOLVO: &str = include_str!("fixtures/mfn_news_volvo.json");
const NEWS_NOVO: &str = include_str!("fixtures/mfn_news_novo.json");
const MFN_FIXTURE: &str = include_str!("fixtures/mfn_gomspace.html");

// ── MFN client (no database) ────────────────────────────────────────────────

#[tokio::test]
async fn mfn_client_honours_retry_after_and_fails_on_other_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a/gomspace-group"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/a/gomspace-group"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let mut c = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    assert_eq!(c.fetch_page("gomspace-group").await.unwrap(), "ok");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let mut c = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    assert!(c.fetch_page("x").await.is_err());
}

#[tokio::test]
async fn mfn_client_spaces_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let mut c = MfnClient::new(server.uri(), Duration::from_millis(300)).unwrap();
    let t = std::time::Instant::now();
    c.fetch_page("a").await.unwrap();
    c.fetch_page("b").await.unwrap();
    assert!(t.elapsed() >= Duration::from_millis(300));
}

// ── MFN feed fixtures: classification (no database) ─────────────────────────

mod fixture {
    use chronicle_lib::nordic::classify::{classify_item, ReportKind};
    use serde_json::Value;

    fn find<'a>(items: &'a [Value], date: &str, lang: &str) -> &'a Value {
        items
            .iter()
            .find(|i| {
                i["content"]["publish_date"]
                    .as_str()
                    .unwrap()
                    .starts_with(date)
                    && i["properties"]["lang"] == lang
            })
            .unwrap_or_else(|| panic!("no {lang} item at {date}"))
    }

    fn classify(item: &Value) -> chronicle_lib::nordic::classify::Classification {
        let tags: Vec<String> = item["properties"]["tags"]
            .as_array()
            .map(|a| a.iter().map(|t| t.as_str().unwrap().to_string()).collect())
            .unwrap_or_default();
        classify_item(&tags, item["content"]["title"].as_str().unwrap())
    }

    /// (publish_date prefix, kind, correction, calendar, fiscal_period)
    #[test]
    fn gomspace_items() {
        let items: Vec<Value> = serde_json::from_str(super::NEWS_GOMSPACE).unwrap();
        let rows = [
            (
                "2026-08-26T06:00",
                Some(ReportKind::Interim),
                false,
                false,
                None,
            ),
            ("2026-05-28", None, false, false, None), // AGM held
            ("2026-04-28", None, false, false, None), // AGM notice
            (
                "2026-02-19T07:00:25",
                Some(ReportKind::Annual),
                false,
                false,
                None,
            ), // untagged Q4
            ("2025-08-27T07:23:05", None, true, false, None), // correction
            (
                "2025-08-27T06:00",
                Some(ReportKind::Interim),
                false,
                false,
                None,
            ),
            ("2025-02-03", None, false, true, None), // financial calendar
            ("2024-01-18", None, false, true, None), // early publication
            ("2019-03-29", None, false, false, None), // annual report document
            (
                "2019-02-28T07:30",
                Some(ReportKind::Annual),
                false,
                false,
                Some("2018"),
            ), // re-send (SQL flags it)
            (
                "2019-02-28T07:02",
                Some(ReportKind::Annual),
                false,
                false,
                Some("2018"),
            ),
        ];
        for (date, kind, corr, cal, fp) in rows {
            let c = classify(find(&items, date, "en"));
            assert_eq!(c.report_kind, kind, "{date} kind");
            assert_eq!(c.is_correction, corr, "{date} correction");
            assert_eq!(c.calendar_update, cal, "{date} calendar");
            assert_eq!(c.fiscal_period.as_deref(), fp, "{date} fiscal period");
        }
    }

    /// Real Novo Nordisk items (`https://mfn.se/all/a/novo-nordisk.json`),
    /// trimmed to the fields the classifier reads.
    #[test]
    fn novo_nordisk_items() {
        let items: Vec<Value> = serde_json::from_str(super::NEWS_NOVO).unwrap();
        let rows = [
            // Q2 2026, published 18:37 Copenhagen time, after the close.
            (
                "2026-08-04T16:37",
                Some(ReportKind::Interim),
                Some("2026-Q2"),
            ),
            ("2026-05-06", Some(ReportKind::Interim), Some("2026-Q1")),
            ("2025-11-05", Some(ReportKind::Interim), Some("2025-Q3")),
            // Year-end results carry only `sub:report:annual`.
            ("2026-02-03", Some(ReportKind::Annual), Some("2025")),
            // The annual report document, its SEC filing and a mis-tagged trial news item.
            ("2026-02-04T06:30", None, None),
            ("2026-02-04T14:53", None, None),
            ("2026-02-02", None, None),
            // Outlook news, AGM, weekly buy-back, insider trades, voting rights, major shareholder.
            ("2026-08-04T16:33", None, None),
            ("2026-03-26", None, None),
            ("2026-10-26", None, None),
            ("2026-10-19", None, None),
            ("2026-08-06", None, None),
            ("2024-04-26", None, None),
            ("2022-05-02", None, None),
            // Untagged trial news whose title says "reports" (false reports before).
            ("2025-11-25", None, None),
            ("2020-06-04", None, None),
        ];
        for (date, kind, fp) in rows {
            let c = classify(find(&items, date, "en"));
            assert_eq!(c.report_kind, kind, "{date} kind");
            assert!(!c.is_correction && !c.calendar_update, "{date}");
            assert_eq!(c.fiscal_period.as_deref(), fp, "{date} fiscal period");
        }
    }

    /// The PDMR notice named in NEX-190 is not in Novo's feed (Novo titles its
    /// notices "Trading in Novo Nordisk shares by board members ..."), so this
    /// one is the wording from the issue, with MFN's report tags on top.
    #[test]
    fn a_pdmr_reporting_notice_is_not_a_report_even_with_report_tags() {
        let title = "Novo Nordisk A/S \u{2013} Reporting of transactions made by persons discharging managerial responsibilities in Novo Nordisk A/S";
        for tags in [
            vec![":regulatory"],
            vec![":regulatory", "sub:report", "sub:report:interim"],
        ] {
            let tags: Vec<String> = tags.into_iter().map(String::from).collect();
            assert_eq!(classify_item(&tags, title).report_kind, None);
        }
    }

    #[test]
    fn volvo_items_in_both_languages() {
        let items: Vec<Value> = serde_json::from_str(super::NEWS_VOLVO).unwrap();
        for lang in ["sv", "en"] {
            let q2 = classify(find(&items, "2026-07-17", lang));
            assert_eq!(q2.report_kind, Some(ReportKind::Interim), "{lang}");
            assert_eq!(q2.fiscal_period.as_deref(), Some("2026-Q2"), "{lang}");
            let q4 = classify(find(&items, "2026-01-28", lang));
            assert_eq!(q4.report_kind, Some(ReportKind::Annual), "{lang}");
            assert_eq!(q4.fiscal_period.as_deref(), Some("2025"), "{lang}");
            let doc = classify(find(&items, "2026-02-26", lang));
            assert_eq!(doc.report_kind, None, "{lang} annual report document");
        }
    }
}

// ── Publishing against a mocked MFN feed (database for the state table) ────

/// Records every (key, event); fails every send once `fail_after` went out.
#[derive(Default)]
struct VecSink {
    sent: Mutex<Vec<(String, SpecialEvent)>>,
    fail_after: Option<usize>,
}

#[async_trait]
impl EventSink for VecSink {
    async fn send(&self, key: &str, event: &SpecialEvent) -> anyhow::Result<()> {
        let mut sent = self.sent.lock().unwrap();
        if self.fail_after.is_some_and(|n| sent.len() >= n) {
            anyhow::bail!("broker down");
        }
        sent.push((key.to_string(), event.clone()));
        Ok(())
    }
}

impl VecSink {
    fn failing_after(n: usize) -> Self {
        Self {
            fail_after: Some(n),
            ..Default::default()
        }
    }

    fn take(&self) -> Vec<(String, SpecialEvent)> {
        std::mem::take(&mut self.sent.lock().unwrap())
    }
}

fn news_items(fixture: &str) -> Vec<Value> {
    serde_json::from_str(fixture).unwrap()
}

/// Serve `items` as a paged feed for `slug`; `next_url` points back at the mock.
async fn serve_feed(server: &MockServer, slug: &str, items: &[Value], per_page: usize) {
    let pages: Vec<&[Value]> = items.chunks(per_page).collect();
    for (n, page) in pages.iter().enumerate() {
        let next = (n + 1 < pages.len()).then(|| {
            format!(
                "{}/all/a/{slug}.json?limit=48&offset={}",
                server.uri(),
                (n + 1) * per_page
            )
        });
        Mock::given(method("GET"))
            .and(path(format!("/all/a/{slug}.json")))
            .and(query_param("offset", (n * per_page).to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "https://www.mfn.se/feed/version/1",
                "items": page,
                "next_url": next,
            })))
            .mount(server)
            .await;
    }
}

async fn serve_calendar(server: &MockServer, slug: &str, html: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/a/{slug}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(html.to_string()))
        .mount(server)
        .await;
}

async fn requests_to(server: &MockServer, prefix: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with(prefix))
        .count()
}

async fn run(db: &TestDb, server: &MockServer, sink: &VecSink, backfill: bool) -> RunSummary {
    let companies = db::load_companies(&db.pool, NORDIC_MICS).await.unwrap();
    let mut feed = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    let mut cal = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    process_companies(&db.pool, &mut feed, &mut cal, sink, &companies, backfill).await
}

async fn cursor(db: &TestDb, ticker: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    db::load_state(
        &db.pool,
        ticker,
        if ticker == "GOMX" { "FNSE" } else { "XSTO" },
    )
    .await
    .unwrap()
    .feed_cursor
}

/// (event_type, occurred_at, lang) of the release events, oldest first.
fn releases(sent: &[(String, SpecialEvent)]) -> Vec<(String, String, String)> {
    let mut out: Vec<_> = sent
        .iter()
        .filter(|(_, e)| e.event_type != "report_calendar")
        .map(|(_, e)| {
            let at = chrono::DateTime::from_timestamp(e.occurred_at_unix_secs, 0).unwrap();
            (
                e.event_type.clone(),
                at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                e.lang.clone(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

fn decoy() -> Value {
    json!({
        "news_id": "11111111-1111-4111-8111-111111111111",
        "url": "https://mfn.se/cis/a/other-co/x",
        "author": {"slug": "other-co", "slugs": ["other-co"], "name": "Other", "tickers": ["XSTO:OTHR"]},
        "properties": {"lang": "en", "tags": [":regulatory", "sub:report", "sub:report:interim"]},
        "content": {"title": "Other Co: Q2 report", "publish_date": "2026-08-26T06:00:00Z"}
    })
}

#[tokio::test]
async fn first_run_publishes_reports_and_corrections_and_a_rerun_only_the_overlap() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    let mut items = news_items(NEWS_GOMSPACE);
    items.insert(2, decoy()); // another issuer's item is never published
    serve_feed(&server, "gomspace-group", &items, 4).await;
    serve_calendar(&server, "gomspace-group", MFN_FIXTURE).await;
    let sink = VecSink::default();

    let first = run(&db, &server, &sink, false).await;
    assert!(first.is_success());
    assert_eq!(
        requests_to(&server, "/all/").await,
        3,
        "no cursor yet: pages to the end"
    );
    // Calendar notices are newer than the (absent) cursor: one calendar refresh.
    assert_eq!(first.calendars_refreshed, vec!["GOMX"]);
    let sent = sink.take();
    assert!(
        sent.iter().all(|(key, _)| key == "GOMX:FNSE"),
        "one key per company"
    );
    assert_eq!(
        releases(&sent),
        vec![
            (
                "annual_report".into(),
                "2019-02-28T07:02:00Z".into(),
                "en".into()
            ),
            (
                "annual_report".into(),
                "2019-02-28T07:30:00Z".into(),
                "en".into()
            ),
            (
                "quarterly_report".into(),
                "2025-08-27T06:00:00Z".into(),
                "en".into()
            ),
            (
                "report_correction".into(),
                "2025-08-27T07:23:05Z".into(),
                "en".into()
            ),
            (
                "annual_report".into(),
                "2026-02-19T07:00:25Z".into(),
                "en".into()
            ),
            (
                "quarterly_report".into(),
                "2026-08-26T06:00:00Z".into(),
                "en".into()
            ),
        ],
        "AGM notices, calendar notices and report documents are not published"
    );
    let calendar: Vec<_> = sent
        .iter()
        .filter(|(_, e)| e.event_type == "report_calendar")
        .collect();
    assert_eq!(calendar.len(), 1);
    assert_eq!(
        cursor(&db, "GOMX").await.unwrap().to_rfc3339(),
        "2026-08-26T06:00:00+00:00"
    );

    // Second run: stops after the first page (it reaches the cursor - 1 day);
    // only the overlap is re-sent and the calendar is not refreshed again.
    let second = run(&db, &server, &sink, false).await;
    assert!(second.is_success() && second.calendars_refreshed.is_empty());
    assert_eq!(requests_to(&server, "/all/").await, 3 + 1);
    assert_eq!(
        releases(&sink.take()),
        vec![(
            "quarterly_report".into(),
            "2026-08-26T06:00:00Z".into(),
            "en".into()
        )]
    );

    // --backfill ignores the cursor.
    run(&db, &server, &sink, true).await;
    assert_eq!(requests_to(&server, "/all/").await, 3 + 1 + 3);
    assert_eq!(releases(&sink.take()).len(), 6);
    db.drop_db().await;
}

#[tokio::test]
async fn a_failed_publish_leaves_the_cursor_and_the_next_run_resends() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve_feed(&server, "gomspace-group", &news_items(NEWS_GOMSPACE), 100).await;
    serve_calendar(&server, "gomspace-group", MFN_FIXTURE).await;

    let flaky = VecSink::failing_after(2);
    let summary = run(&db, &server, &flaky, false).await;
    assert_eq!((summary.attempted, summary.failed), (1, 1));
    assert_eq!(cursor(&db, "GOMX").await, None, "cursor not moved");

    let ok = VecSink::default();
    assert!(run(&db, &server, &ok, false).await.is_success());
    assert_eq!(releases(&ok.take()).len(), 6, "everything re-sent");
    assert!(cursor(&db, "GOMX").await.is_some());
    db.drop_db().await;
}

#[tokio::test]
async fn only_a_calendar_notice_newer_than_the_cursor_triggers_a_refresh() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve_feed(&server, "gomspace-group", &news_items(NEWS_GOMSPACE), 100).await;
    serve_calendar(&server, "gomspace-group", MFN_FIXTURE).await;
    // The fixture's newest calendar notice is from 2025-02-03.
    let older = chrono::DateTime::parse_from_rfc3339("2025-03-01T00:00:00Z").unwrap();
    db::advance_feed_cursor(&db.pool, "GOMX", "FNSE", older.into())
        .await
        .unwrap();
    let sink = VecSink::default();
    let summary = run(&db, &server, &sink, false).await;
    assert!(summary.calendar_updated.is_empty());
    assert_eq!(requests_to(&server, "/a/").await, 0);

    sqlx::query("DELETE FROM mfn_ingest_state")
        .execute(&db.pool)
        .await
        .unwrap();
    let before = chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z").unwrap();
    db::advance_feed_cursor(&db.pool, "GOMX", "FNSE", before.into())
        .await
        .unwrap();
    let summary = run(&db, &server, &sink, false).await;
    assert_eq!(summary.calendar_updated, vec!["GOMX"]);
    assert_eq!(requests_to(&server, "/a/").await, 1);
    db.drop_db().await;
}

#[tokio::test]
async fn both_languages_are_published_and_a_failing_company_does_not_stop_the_others() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.add_company("VOLV B", "XSTO", "volvo").await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/all/a/gomspace-group.json"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    serve_feed(&server, "volvo", &news_items(NEWS_VOLVO), 100).await;
    let sink = VecSink::default();

    let summary = run(&db, &server, &sink, false).await;
    assert_eq!((summary.attempted, summary.failed), (2, 1));
    let sent = sink.take();
    assert!(sent.iter().all(|(key, _)| key == "VOLV B:XSTO"));
    // sv and en of both reports; signal merges the twins.
    let langs: Vec<String> = releases(&sent).into_iter().map(|r| r.2).collect();
    assert_eq!(langs.len(), 4);
    assert_eq!(langs.iter().filter(|l| *l == "en").count(), 2);
    db.drop_db().await;
}

#[tokio::test]
async fn rate_limited_request_is_retried() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/all/a/gomspace-group.json"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    serve_feed(&server, "gomspace-group", &news_items(NEWS_GOMSPACE), 100).await;
    serve_calendar(&server, "gomspace-group", MFN_FIXTURE).await;
    let sink = VecSink::default();

    let started = std::time::Instant::now();
    assert!(run(&db, &server, &sink, false).await.is_success());
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(requests_to(&server, "/all/").await, 2);
    assert_eq!(releases(&sink.take()).len(), 6);
    db.drop_db().await;
}

/// Novo Nordisk (Nasdaq Copenhagen): a Danish feed page publishes exactly the
/// reports, keyed `NOVO-B:XCSE`, and the company's ISIN matches the feed's.
#[tokio::test]
async fn danish_company_publishes_exactly_its_reports() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.add_company_with_isin("NOVO-B", "XCSE", "novo-nordisk", "DKK", "DK0062498333")
        .await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/all/a/gomspace-group.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(&server)
        .await;
    serve_feed(&server, "novo-nordisk", &news_items(NEWS_NOVO), 5).await;
    serve_calendar(
        &server,
        "novo-nordisk",
        include_str!("fixtures/mfn_novo.html"),
    )
    .await;
    let sink = VecSink::default();

    let summary = run(&db, &server, &sink, false).await;
    assert!(summary.is_success(), "{summary:?}");
    let sent = sink.take();
    assert!(
        sent.iter().all(|(key, _)| key == "NOVO-B:XCSE"),
        "one key per company: {:?}",
        sent.iter().map(|(k, _)| k).collect::<Vec<_>>()
    );
    assert!(sent
        .iter()
        .all(|(_, e)| e.ticker == "NOVO-B" && e.exchange_mic == "XCSE"));
    assert_eq!(
        releases(&sent),
        vec![
            (
                "quarterly_report".into(),
                "2025-11-05T06:30:45Z".into(),
                "en".into()
            ),
            (
                "annual_report".into(),
                "2026-02-03T16:38:42Z".into(),
                "en".into()
            ),
            (
                "quarterly_report".into(),
                "2026-05-06T05:31:16Z".into(),
                "en".into()
            ),
            (
                "quarterly_report".into(),
                "2026-08-04T16:37:02Z".into(),
                "en".into()
            ),
        ],
        "buy-backs, insider trades, AGM, the annual report document and news are not published"
    );
    // The stored ISIN stays: MFN lists the same one.
    let isin: Option<String> =
        sqlx::query_scalar("SELECT isin FROM companies WHERE ticker = 'NOVO-B'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(isin.as_deref(), Some("DK0062498333"));
    db.drop_db().await;
}

// ── Calendar snapshots ──────────────────────────────────────────────────────

async fn gomx(db: &TestDb) -> Company {
    db::load_mapped_companies(&db.pool)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.ticker == "GOMX")
        .unwrap()
}

#[tokio::test]
async fn an_unchanged_calendar_is_not_republished_unless_forced() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve_calendar(&server, "gomspace-group", MFN_FIXTURE).await;
    let mut client = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    let company = gomx(&db).await;
    let sink = VecSink::default();

    let first = refresh_company(&db.pool, &mut client, &sink, &company, false)
        .await
        .unwrap();
    assert!(matches!(first, Refresh::Published(n) if n > 0));
    let sent = sink.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "GOMX:FNSE");
    let snapshot = sent[0].1.calendar.as_ref().unwrap();
    assert!(snapshot.entries.iter().all(|e| !e.expected_date.is_empty()));

    let again = refresh_company(&db.pool, &mut client, &sink, &company, false)
        .await
        .unwrap();
    assert_eq!(again, Refresh::Unchanged);
    assert!(sink.take().is_empty());
    let forced = refresh_company(&db.pool, &mut client, &sink, &company, true)
        .await
        .unwrap();
    assert!(matches!(forced, Refresh::Published(_)));
    assert_eq!(sink.take().len(), 1);
    db.drop_db().await;
}

#[tokio::test]
async fn a_changed_calendar_is_published_and_a_parse_failure_publishes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let company = gomx(&db).await;
    let sink = VecSink::default();
    db::set_calendar_hash(&db.pool, "GOMX", "FNSE", "an older calendar")
        .await
        .unwrap();

    let server = MockServer::start().await;
    serve_calendar(&server, "gomspace-group", MFN_FIXTURE).await;
    let mut client = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    let changed = refresh_company(&db.pool, &mut client, &sink, &company, false)
        .await
        .unwrap();
    assert!(matches!(changed, Refresh::Published(_)));
    sink.take();
    let stored = db::load_state(&db.pool, "GOMX", "FNSE")
        .await
        .unwrap()
        .calendar_hash;

    let broken = MockServer::start().await;
    serve_calendar(
        &broken,
        "gomspace-group",
        "<html><body>maintenance</body></html>",
    )
    .await;
    let mut client = MfnClient::new(broken.uri(), Duration::ZERO).unwrap();
    assert!(
        refresh_company(&db.pool, &mut client, &sink, &company, true)
            .await
            .is_err()
    );
    assert!(sink.take().is_empty());
    let kept = db::load_state(&db.pool, "GOMX", "FNSE")
        .await
        .unwrap()
        .calendar_hash;
    assert_eq!(kept, stored);
    db.drop_db().await;
}

#[tokio::test]
async fn a_failed_calendar_publish_keeps_the_old_hash() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve_calendar(&server, "gomspace-group", MFN_FIXTURE).await;
    let mut client = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    let company = gomx(&db).await;

    let down = VecSink::failing_after(0);
    assert!(
        refresh_company(&db.pool, &mut client, &down, &company, false)
            .await
            .is_err()
    );
    assert_eq!(
        db::load_state(&db.pool, "GOMX", "FNSE")
            .await
            .unwrap()
            .calendar_hash,
        None
    );
    let ok = VecSink::default();
    let retried = refresh_company(&db.pool, &mut client, &ok, &company, false)
        .await
        .unwrap();
    assert!(matches!(retried, Refresh::Published(_)));
    db.drop_db().await;
}
