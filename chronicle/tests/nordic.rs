//! Database-backed integration tests for the Nordic report tables. Skipped
//! unless `TEST_DATABASE_URL` is set (the smoke-test workflow sets it).

mod common;

use chronicle_lib::nordic::calendar::{
    apply_calendar, classify_label, refresh_company, CalendarEntry, LabelClass, MfnClient,
    ParseError, Refresh,
};
use chronicle_lib::nordic::db::{self, record_release, NewRelease, Recorded};
use chronicle_lib::nordic::ingest::process_companies;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use common::TestDb;
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use std::time::Duration;
use uuid::Uuid;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NEWS_GOMSPACE: &str = include_str!("fixtures/mfn_news_gomspace.json");
const NEWS_VOLVO: &str = include_str!("fixtures/mfn_news_volvo.json");
const MFN_FIXTURE: &str = include_str!("fixtures/mfn_gomspace.html");

/// Insert a scheduled (unreleased) row; returns its id.
async fn schedule(pool: &PgPool, date: &str, label: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO report_releases \
             (ticker, exchange_mic, expected_date, expected_time, calendar_label, fiscal_period) \
             VALUES ('GOMX','FNSE',$1::date,'08:00',$2,'Q2') RETURNING id",
    )
    .bind(date)
    .bind(label)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn uid(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn release(n: u128, released_at: DateTime<Utc>) -> NewRelease<'static> {
    NewRelease {
        ticker: "GOMX",
        exchange_mic: "FNSE",
        mfn_news_id: uid(n),
        released_at,
        report_kind: Some("interim"),
        headline: "Q2 report",
        url: Some("https://example.test/x"),
        lang: Some("en"),
        tags: &[],
        fiscal_period: None,
        is_correction: false,
        calendar_update: false,
    }
}

fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
}

async fn count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM report_releases")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn release_matches_scheduled_row_and_keeps_expected_fields() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let id = schedule(&db.pool, "2026-08-27", "Q2 report").await;
    let got = record_release(&db.pool, &release(10, at(2026, 8, 26, 6)))
        .await
        .unwrap();
    assert_eq!(got, Recorded::Matched("2026-08-27".parse().unwrap()));
    assert_eq!(count(&db.pool).await, 1);
    let row = sqlx::query(
        "SELECT mfn_news_id, released_at, expected_date::text AS d, \
                    expected_time::text AS t, calendar_label, fiscal_period, report_kind, headline \
             FROM report_releases WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Option<Uuid>, _>("mfn_news_id"), Some(uid(10)));
    assert_eq!(
        row.get::<Option<DateTime<Utc>>, _>("released_at"),
        Some(at(2026, 8, 26, 6))
    );
    assert_eq!(row.get::<String, _>("d"), "2026-08-27");
    assert_eq!(row.get::<String, _>("t"), "08:00:00");
    assert_eq!(row.get::<String, _>("calendar_label"), "Q2 report");
    assert_eq!(row.get::<String, _>("fiscal_period"), "Q2");
    assert_eq!(row.get::<String, _>("report_kind"), "interim");
    db.drop_db().await;
}

#[tokio::test]
async fn release_without_scheduled_row_inserts() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let got = record_release(&db.pool, &release(11, at(2026, 8, 26, 6)))
        .await
        .unwrap();
    assert_eq!(got, Recorded::Inserted);
    let expected: Option<NaiveDate> =
        sqlx::query_scalar("SELECT expected_date FROM report_releases")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(expected, None);
    db.drop_db().await;
}

#[tokio::test]
async fn closest_scheduled_date_wins_and_ties_pick_earlier() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let far = schedule(&db.pool, "2026-08-31", "far").await;
    let near = schedule(&db.pool, "2026-08-27", "near").await;
    let got = record_release(&db.pool, &release(12, at(2026, 8, 26, 6)))
        .await
        .unwrap();
    assert_eq!(got, Recorded::Matched("2026-08-27".parse().unwrap()));
    let matched: Option<i64> =
        sqlx::query_scalar("SELECT id FROM report_releases WHERE mfn_news_id = $1")
            .bind(uid(12))
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(matched, Some(near));
    assert_ne!(matched, Some(far));

    // Tie: 2 days either side of 2026-11-10 → the earlier one.
    let early = schedule(&db.pool, "2026-11-08", "early").await;
    schedule(&db.pool, "2026-11-12", "late").await;
    record_release(&db.pool, &release(13, at(2026, 11, 10, 7)))
        .await
        .unwrap();
    let matched: i64 = sqlx::query_scalar("SELECT id FROM report_releases WHERE mfn_news_id = $1")
        .bind(uid(13))
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(matched, early);
    db.drop_db().await;
}

#[tokio::test]
async fn candidate_outside_seven_days_is_not_matched() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    schedule(&db.pool, "2026-09-05", "too far").await; // 10 days after
    let got = record_release(&db.pool, &release(14, at(2026, 8, 26, 6)))
        .await
        .unwrap();
    assert_eq!(got, Recorded::Inserted);
    assert_eq!(count(&db.pool).await, 2);
    // Exactly 7 days apart still matches.
    schedule(&db.pool, "2026-10-07", "edge").await;
    let got = record_release(&db.pool, &release(15, at(2026, 9, 30, 6)))
        .await
        .unwrap();
    assert!(matches!(got, Recorded::Matched(_)));
    db.drop_db().await;
}

#[tokio::test]
async fn match_uses_stockholm_local_date() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // 23:30Z on 2026-08-20 is 01:30 on 2026-08-21 in Stockholm; a row
    // expected 2026-08-28 is 7 local days away (matches), 8 UTC days away.
    schedule(&db.pool, "2026-08-28", "edge").await;
    let got = record_release(
        &db.pool,
        &release(16, Utc.with_ymd_and_hms(2026, 8, 20, 23, 30, 0).unwrap()),
    )
    .await
    .unwrap();
    assert!(matches!(got, Recorded::Matched(_)));
    db.drop_db().await;
}

#[tokio::test]
async fn corrections_non_reports_and_calendar_updates_never_match() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    schedule(&db.pool, "2026-08-27", "Q2 report").await;
    let mut correction = release(20, at(2026, 8, 26, 7));
    correction.is_correction = true;
    correction.report_kind = None;
    let mut non_report = release(21, at(2026, 8, 26, 8));
    non_report.report_kind = None;
    let mut calendar = release(22, at(2026, 8, 26, 9));
    calendar.report_kind = None;
    calendar.calendar_update = true;
    // Even with report_kind set, a correction / calendar flag blocks matching.
    let mut flagged = release(23, at(2026, 8, 26, 10));
    flagged.is_correction = true;
    for r in [&correction, &non_report, &calendar, &flagged] {
        assert_eq!(
            record_release(&db.pool, r).await.unwrap(),
            Recorded::Inserted
        );
    }
    let unreleased: i64 =
        sqlx::query_scalar("SELECT count(*) FROM report_releases WHERE released_at IS NULL")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(unreleased, 1);
    db.drop_db().await;
}

#[tokio::test]
async fn rerunning_the_same_feed_creates_no_duplicates() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    schedule(&db.pool, "2026-08-27", "Q2 report").await;
    let r = release(30, at(2026, 8, 26, 6));
    assert!(matches!(
        record_release(&db.pool, &r).await.unwrap(),
        Recorded::Matched(_)
    ));
    assert_eq!(
        record_release(&db.pool, &r).await.unwrap(),
        Recorded::Updated
    );
    let other = release(31, at(2026, 5, 7, 6));
    assert_eq!(
        record_release(&db.pool, &other).await.unwrap(),
        Recorded::Inserted
    );
    assert_eq!(
        record_release(&db.pool, &other).await.unwrap(),
        Recorded::Updated
    );
    assert_eq!(count(&db.pool).await, 2);
    db.drop_db().await;
}

#[tokio::test]
async fn check_constraints_reject_invalid_rows() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // Neither a release instant nor an expected date.
    let neither =
        sqlx::query("INSERT INTO report_releases (ticker, exchange_mic) VALUES ('GOMX','FNSE')")
            .execute(&db.pool)
            .await;
    assert!(neither.is_err());
    // mfn_news_id without released_at.
    let disclosure_only = sqlx::query(
        "INSERT INTO report_releases (ticker, exchange_mic, mfn_news_id, expected_date) \
             VALUES ('GOMX','FNSE','00000000-0000-0000-0000-000000000063','2026-08-27')",
    )
    .execute(&db.pool)
    .await;
    assert!(disclosure_only.is_err());
    // released_at without mfn_news_id.
    let released_only = sqlx::query(
        "INSERT INTO report_releases (ticker, exchange_mic, released_at) \
             VALUES ('GOMX','FNSE', now())",
    )
    .execute(&db.pool)
    .await;
    assert!(released_only.is_err());
    // Duplicate scheduled row is rejected by the partial unique index.
    schedule(&db.pool, "2026-08-27", "dup").await;
    let dup = sqlx::query(
        "INSERT INTO report_releases (ticker, exchange_mic, expected_date, calendar_label) \
             VALUES ('GOMX','FNSE','2026-08-27','dup')",
    )
    .execute(&db.pool)
    .await;
    assert!(dup.is_err());
    db.drop_db().await;
}

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

// ── Calendar side (database) ────────────────────────────────────────────────

fn entry(date: &str, label: &str) -> CalendarEntry {
    match classify_label(label) {
        LabelClass::Report(kind, fiscal_period) => CalendarEntry {
            date: date.parse().unwrap(),
            time: None,
            label: label.into(),
            kind,
            fiscal_period,
        },
        other => panic!("not a report label: {other:?}"),
    }
}

fn day(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

async fn scheduled_labels(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT calendar_label FROM report_releases WHERE released_at IS NULL ORDER BY expected_date",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn calendar_replaces_only_future_scheduled_rows() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    schedule(&db.pool, "2026-01-10", "past, never seen").await; // date passed: kept
    schedule(&db.pool, "2027-01-01", "moved away").await; // future: replaced
    let out = apply_calendar(
        &db.pool,
        "GOMX",
        "FNSE",
        day("2026-10-21"),
        &[entry("2027-02-18", "Bokslutskommuniké 2026")],
    )
    .await
    .unwrap();
    assert_eq!((out.scheduled, out.matched_released), (1, 0));
    assert_eq!(
        scheduled_labels(&db.pool).await,
        vec!["past, never seen", "Bokslutskommuniké 2026"]
    );
    let (kind, period): (String, String) = sqlx::query_as(
        "SELECT report_kind, fiscal_period FROM report_releases WHERE calendar_label = 'Bokslutskommuniké 2026'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!((kind.as_str(), period.as_str()), ("annual", "2026"));

    // Applying the same calendar again changes nothing (idempotent).
    apply_calendar(
        &db.pool,
        "GOMX",
        "FNSE",
        day("2026-10-21"),
        &[entry("2027-02-18", "Bokslutskommuniké 2026")],
    )
    .await
    .unwrap();
    assert_eq!(scheduled_labels(&db.pool).await.len(), 2);

    // A valid, empty upcoming list clears the future rows only.
    apply_calendar(&db.pool, "GOMX", "FNSE", day("2026-10-21"), &[])
        .await
        .unwrap();
    assert_eq!(scheduled_labels(&db.pool).await, vec!["past, never seen"]);
    db.drop_db().await;
}

#[tokio::test]
async fn calendar_entry_matches_an_already_released_report() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // Released 08:00 local (06:00Z) on the calendar date; MFN still lists it as upcoming.
    let rel = release(40, at(2026, 10, 23, 6));
    assert_eq!(
        record_release(&db.pool, &rel).await.unwrap(),
        Recorded::Inserted
    );
    let out = apply_calendar(
        &db.pool,
        "GOMX",
        "FNSE",
        day("2026-10-23"),
        &[entry("2026-10-23", "Kvartalsrapport 2026-Q3")],
    )
    .await
    .unwrap();
    assert_eq!((out.scheduled, out.matched_released), (0, 1));
    assert_eq!(count(&db.pool).await, 1);
    let row = sqlx::query(
        "SELECT mfn_news_id, expected_date::text AS d, calendar_label, fiscal_period \
         FROM report_releases",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Option<Uuid>, _>("mfn_news_id"), Some(uid(40)));
    assert_eq!(row.get::<String, _>("d"), "2026-10-23");
    assert_eq!(
        row.get::<String, _>("calendar_label"),
        "Kvartalsrapport 2026-Q3"
    );
    assert_eq!(row.get::<String, _>("fiscal_period"), "2026-Q3");

    // A second entry cannot claim the same, already matched, release.
    let out = apply_calendar(
        &db.pool,
        "GOMX",
        "FNSE",
        day("2026-10-23"),
        &[
            entry("2026-10-23", "Kvartalsrapport 2026-Q3"),
            entry("2026-10-24", "Delårsrapport 2026-Q3"),
        ],
    )
    .await
    .unwrap();
    assert_eq!((out.scheduled, out.matched_released), (2, 0));
    db.drop_db().await;
}

#[tokio::test]
async fn refresh_never_deletes_or_changes_released_rows() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // Released row that already carries expected fields from an earlier match.
    schedule(&db.pool, "2026-08-27", "Kvartalsrapport 2026-Q2").await;
    record_release(&db.pool, &release(50, at(2026, 8, 26, 6)))
        .await
        .unwrap();
    // Released row far from anything the calendar will list, no expected fields.
    record_release(&db.pool, &release(51, at(2026, 2, 19, 7)))
        .await
        .unwrap();

    let snapshot = |pool: PgPool| async move {
        sqlx::query(
            "SELECT id, mfn_news_id, released_at, expected_date, expected_time, calendar_label, \
                    fiscal_period, report_kind, headline, fetched_at \
             FROM report_releases WHERE released_at IS NOT NULL ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
                r.get::<i64, _>("id"),
                r.get::<Option<Uuid>, _>("mfn_news_id"),
                r.get::<Option<DateTime<Utc>>, _>("released_at"),
                r.get::<Option<NaiveDate>, _>("expected_date"),
                r.get::<Option<String>, _>("calendar_label"),
                r.get::<Option<String>, _>("fiscal_period"),
                r.get::<Option<String>, _>("report_kind"),
                r.get::<Option<String>, _>("headline"),
                r.get::<DateTime<Utc>, _>("fetched_at"),
            )
        })
        .collect::<Vec<_>>()
    };
    let before = snapshot(db.pool.clone()).await;
    assert_eq!(before.len(), 2);

    for entries in [vec![], vec![entry("2027-04-29", "Kvartalsrapport 2027-Q1")]] {
        apply_calendar(&db.pool, "GOMX", "FNSE", day("2026-10-21"), &entries)
            .await
            .unwrap();
        assert_eq!(snapshot(db.pool.clone()).await, before);
    }
    db.drop_db().await;
}

#[tokio::test]
async fn refresh_company_applies_fixture_and_parse_failure_keeps_rows() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a/gomspace-group"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MFN_FIXTURE))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/a/gomspace-group"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("<html><body>Redesigned</body></html>"),
        )
        .mount(&server)
        .await;
    let mut client = MfnClient::new(server.uri(), Duration::ZERO).unwrap();

    let first = refresh_company(&db.pool, &mut client, "GOMX", "FNSE")
        .await
        .unwrap();
    assert!(matches!(first, Refresh::Refreshed(o) if o.scheduled == 1));
    assert_eq!(
        scheduled_labels(&db.pool).await,
        vec!["Bokslutskommuniké 2026"]
    );

    // Page changed: error, rows kept.
    let err = refresh_company(&db.pool, &mut client, "GOMX", "FNSE")
        .await
        .unwrap_err();
    assert_eq!(err.downcast_ref::<ParseError>(), Some(&ParseError::NoTable));
    assert_eq!(
        scheduled_labels(&db.pool).await,
        vec!["Bokslutskommuniké 2026"]
    );

    // No slug: nothing fetched.
    sqlx::query("UPDATE companies SET mfn_slug = NULL")
        .execute(&db.pool)
        .await
        .unwrap();
    let before = server.received_requests().await.unwrap().len();
    assert_eq!(
        refresh_company(&db.pool, &mut client, "GOMX", "FNSE")
            .await
            .unwrap(),
        Refresh::NoSlug
    );
    assert_eq!(server.received_requests().await.unwrap().len(), before);
    db.drop_db().await;
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

// ── nordic-reports against a mocked MFN feed (database) ─────────────────────

const GOMSPACE_REPORTS: [(&str, &str, &str); 4] = [
    ("2019-02-28T07:02:00Z", "annual", "en"),
    ("2025-08-27T06:00:00Z", "interim", "en"),
    ("2026-02-19T07:00:25Z", "annual", "en"),
    ("2026-08-26T06:00:00Z", "interim", "en"),
];

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

async fn serve_calendar(server: &MockServer, slug: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/a/{slug}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(MFN_FIXTURE))
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

async fn run(
    db: &TestDb,
    server: &MockServer,
    backfill: bool,
) -> chronicle_lib::nordic::ingest::RunSummary {
    let companies = db::load_companies(&db.pool, &["FNSE", "XSTO"])
        .await
        .unwrap();
    let mut feed = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    let mut cal = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    process_companies(&db.pool, &mut feed, &mut cal, &companies, backfill).await
}

async fn report_rows(pool: &PgPool, ticker: &str) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT to_char(released_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                report_kind, lang \
         FROM report_releases \
         WHERE ticker = $1 AND report_kind IS NOT NULL AND released_at IS NOT NULL ORDER BY released_at",
    )
    .bind(ticker)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Everything except `fetched_at`, for before/after comparisons.
async fn snapshot(pool: &PgPool) -> Vec<String> {
    sqlx::query(
        "SELECT id, mfn_news_id, released_at, expected_date, report_kind, headline, lang, tags, \
                is_correction, calendar_update, superseded_by, fiscal_period \
         FROM report_releases ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            r.get::<i64, _>("id"),
            r.get::<Option<Uuid>, _>("mfn_news_id"),
            r.get::<Option<DateTime<Utc>>, _>("released_at"),
            r.get::<Option<NaiveDate>, _>("expected_date"),
            r.get::<Option<String>, _>("report_kind"),
            r.get::<Option<String>, _>("headline"),
            r.get::<Option<String>, _>("lang"),
            r.get::<Vec<String>, _>("tags"),
            r.get::<bool, _>("is_correction"),
            r.get::<bool, _>("calendar_update"),
            r.get::<Option<i64>, _>("superseded_by"),
            r.get::<Option<String>, _>("fiscal_period"),
        )
    })
    .collect()
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
async fn first_run_inserts_expected_rows_second_run_is_a_noop() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    let mut items = news_items(NEWS_GOMSPACE);
    items.insert(2, decoy()); // another issuer's item is never stored
    serve_feed(&server, "gomspace-group", &items, 4).await;
    serve_calendar(&server, "gomspace-group").await;

    let first = run(&db, &server, false).await;
    assert_eq!((first.attempted, first.failed), (1, 0));
    assert!(first.is_success());
    assert_eq!(first.calendar_updated, vec!["GOMX"]);
    assert_eq!(first.calendars_refreshed, vec!["GOMX"]);
    assert_eq!(requests_to(&server, "/all/").await, 3); // first run reads every page
    assert_eq!(requests_to(&server, "/a/").await, 1);

    let rows: Vec<_> = GOMSPACE_REPORTS
        .iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
        .collect();
    assert_eq!(report_rows(&db.pool, "GOMX").await, rows);
    // 11 fixture items; the scheduled row comes from the calendar refresh.
    let released: i64 =
        sqlx::query_scalar("SELECT count(*) FROM report_releases WHERE released_at IS NOT NULL")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(released, 11);
    // Re-sends and corrections are flagged, never counted, and linked from the originals.
    let corrections: Vec<String> = sqlx::query_scalar(
        "SELECT to_char(released_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
         FROM report_releases WHERE is_correction ORDER BY released_at",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        corrections,
        vec!["2019-02-28T07:30:00Z", "2025-08-27T07:23:05Z"]
    );
    let linked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM report_releases o JOIN report_releases c ON c.id = o.superseded_by \
         WHERE c.is_correction AND o.report_kind IS NOT NULL",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(linked, 2);
    let calendar: i64 =
        sqlx::query_scalar("SELECT count(*) FROM report_releases WHERE calendar_update")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(calendar, 2);

    // Second run: no change, paging stops after the first page, no calendar fetch.
    let before = snapshot(&db.pool).await;
    let second = run(&db, &server, false).await;
    assert!(second.is_success());
    assert!(second.calendar_updated.is_empty() && second.calendars_refreshed.is_empty());
    assert_eq!(requests_to(&server, "/all/").await, 3 + 1);
    assert_eq!(requests_to(&server, "/a/").await, 1);
    assert_eq!(snapshot(&db.pool).await, before);

    // --backfill pages to the end again and is still a no-op.
    run(&db, &server, true).await;
    assert_eq!(requests_to(&server, "/all/").await, 3 + 1 + 3);
    assert_eq!(snapshot(&db.pool).await, before);
    db.drop_db().await;
}

#[tokio::test]
async fn language_pair_yields_one_english_row_whichever_order() {
    for reverse in [false, true] {
        let Some(db) = TestDb::create().await else {
            return;
        };
        db.add_company("VOLV B", "XSTO", "volvo").await;
        let server = MockServer::start().await;
        let mut items = news_items(NEWS_VOLVO);
        items.sort_by_key(|i| {
            (
                i["content"]["publish_date"].as_str().unwrap().to_string(),
                i["properties"]["lang"] == "en",
            )
        });
        if reverse {
            items.reverse();
        }
        serve_feed(&server, "volvo", &items, 100).await;
        // GOMX is also registered; give it an empty feed.
        Mock::given(method("GET"))
            .and(path("/all/a/gomspace-group.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": null})))
            .mount(&server)
            .await;

        for _ in 0..2 {
            let summary = run(&db, &server, false).await;
            assert!(summary.is_success());
            assert_eq!(
                report_rows(&db.pool, "VOLV B").await,
                vec![
                    ("2026-01-28T06:20:00Z".into(), "annual".into(), "en".into()),
                    ("2026-07-17T05:20:00Z".into(), "interim".into(), "en".into()),
                ],
                "reverse={reverse}"
            );
            // 2 report rows + the annual-report document in both languages.
            assert_eq!(count(&db.pool).await, 4, "reverse={reverse}");
        }
        let doc_kinds: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM report_releases WHERE report_kind IS NULL AND NOT is_correction",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(doc_kinds, 2);
        db.drop_db().await;
    }
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
    serve_calendar(&server, "gomspace-group").await;

    let started = std::time::Instant::now();
    let summary = run(&db, &server, false).await;
    assert!(summary.is_success());
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(requests_to(&server, "/all/").await, 2);
    assert_eq!(report_rows(&db.pool, "GOMX").await.len(), 4);
    db.drop_db().await;
}

#[tokio::test]
async fn a_failing_company_does_not_stop_the_others_but_fails_the_run() {
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

    let summary = run(&db, &server, false).await;
    assert_eq!((summary.attempted, summary.failed), (2, 1));
    assert!(!summary.is_success());
    assert_eq!(report_rows(&db.pool, "VOLV B").await.len(), 2);
    assert!(report_rows(&db.pool, "GOMX").await.is_empty());
    db.drop_db().await;
}

#[tokio::test]
async fn ingest_attaches_to_a_scheduled_row() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let id = schedule(&db.pool, "2026-08-25", "Kvartalsrapport 2026-Q2").await;
    let server = MockServer::start().await;
    serve_feed(&server, "gomspace-group", &news_items(NEWS_GOMSPACE), 100).await;
    serve_calendar(&server, "gomspace-group").await;
    run(&db, &server, false).await;

    let row = sqlx::query(
        "SELECT released_at, report_kind, expected_date::text AS d, calendar_label, fiscal_period \
         FROM report_releases WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        row.get::<Option<DateTime<Utc>>, _>("released_at"),
        Some(at(2026, 8, 26, 6))
    );
    assert_eq!(
        row.get::<Option<String>, _>("report_kind").as_deref(),
        Some("interim")
    );
    assert_eq!(row.get::<String, _>("d"), "2026-08-25");
    assert_eq!(
        row.get::<String, _>("calendar_label"),
        "Kvartalsrapport 2026-Q2"
    );
    // The calendar's period (here "Q2") wins over the classifier's guess; never overwritten.
    assert_eq!(row.get::<String, _>("fiscal_period"), "Q2");
    db.drop_db().await;
}

#[tokio::test]
async fn same_day_resends_become_corrections_idempotently() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let first = release(70, at(2026, 8, 26, 6));
    let mut resend = release(71, at(2026, 8, 26, 8));
    resend.headline = "Q2 report (re-sent)";
    let next_day = release(72, at(2026, 8, 27, 6));
    for r in [&first, &resend, &next_day] {
        assert_eq!(
            record_release(&db.pool, r).await.unwrap(),
            Recorded::Inserted
        );
    }
    for _ in 0..2 {
        db::flag_resends(&db.pool, "GOMX", "FNSE").await.unwrap();
        db::link_corrections(&db.pool, "GOMX", "FNSE")
            .await
            .unwrap();
        let rows: Vec<(String, bool, Option<String>, bool)> = sqlx::query_as(
            "SELECT headline, is_correction, report_kind, superseded_by IS NOT NULL \
             FROM report_releases ORDER BY released_at",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("Q2 report".into(), false, Some("interim".into()), true),
                ("Q2 report (re-sent)".into(), true, None, false),
                ("Q2 report".into(), false, Some("interim".into()), false),
            ]
        );
    }
    // Re-serving the re-sent item doesn't resurrect it as a report.
    assert_eq!(
        record_release(&db.pool, &resend).await.unwrap(),
        Recorded::Updated
    );
    let kind: Option<String> =
        sqlx::query_scalar("SELECT report_kind FROM report_releases WHERE mfn_news_id = $1")
            .bind(uid(71))
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(kind, None);
    db.drop_db().await;
}

// ── Migration from the Nasdaq-era schema (database) ─────────────────────────

#[tokio::test]
async fn migration_replaces_nasdaq_rows_and_keeps_calendar_data() {
    let Some(db) = TestDb::create_empty().await else {
        return;
    };
    const NEW_VERSION: i64 = 20261023000000;
    let mut partial = sqlx::migrate!("./migrations");
    partial.migrations = partial
        .migrations
        .iter()
        .filter(|m| m.version < NEW_VERSION)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    partial.run(&db.pool).await.unwrap();

    sqlx::query(
        "INSERT INTO companies (ticker, sector, exchange_mic, currency, mfn_slug, nasdaq_news_company) \
         VALUES ('GOMX','Technology','FNSE','SEK','gomspace-group','GomSpace Group AB')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    // Old schema: disclosure_id / category.
    let released = "INSERT INTO report_releases (ticker, exchange_mic, disclosure_id, released_at, \
         category, report_kind, headline, url, is_correction, expected_date, expected_time, \
         calendar_label, fiscal_period) \
         VALUES ('GOMX','FNSE',$1,$2,'Interim report (Q1 and Q3)',$3,'headline','http://u',$4,$5,$6,$7,$8) \
         RETURNING id";
    let plain: i64 = sqlx::query_scalar(released)
        .bind(1_i64)
        .bind(at(2025, 8, 27, 6))
        .bind("interim")
        .bind(false)
        .bind(None::<NaiveDate>)
        .bind(None::<chrono::NaiveTime>)
        .bind(None::<&str>)
        .bind(None::<&str>)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let correction: i64 = sqlx::query_scalar(released)
        .bind(2_i64)
        .bind(at(2025, 8, 27, 7))
        .bind(None::<&str>)
        .bind(true)
        .bind(None::<NaiveDate>)
        .bind(None::<chrono::NaiveTime>)
        .bind(None::<&str>)
        .bind(None::<&str>)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE report_releases SET superseded_by = $1 WHERE id = $2")
        .bind(correction)
        .bind(plain)
        .execute(&db.pool)
        .await
        .unwrap();
    let with_calendar: i64 = sqlx::query_scalar(released)
        .bind(3_i64)
        .bind(at(2026, 8, 26, 6))
        .bind("interim")
        .bind(false)
        .bind(Some(day("2026-08-27")))
        .bind(Some(chrono::NaiveTime::from_hms_opt(8, 0, 0).unwrap()))
        .bind(Some("Kvartalsrapport 2026-Q2"))
        .bind(Some("2026-Q2"))
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let scheduled: i64 = sqlx::query_scalar(
        "INSERT INTO report_releases (ticker, exchange_mic, expected_date, expected_time, \
         calendar_label, report_kind, fiscal_period) \
         VALUES ('GOMX','FNSE','2027-02-18','08:30','Bokslutskommuniké 2026','annual','2026') RETURNING id",
    )
    .fetch_one(&db.pool).await.unwrap();
    // Would collide with `with_calendar` once that row is a scheduled row again.
    let collider: i64 = sqlx::query_scalar(
        "INSERT INTO report_releases (ticker, exchange_mic, expected_date, calendar_label, report_kind) \
         VALUES ('GOMX','FNSE','2026-08-27','Kvartalsrapport 2026-Q2','interim') RETURNING id",
    )
    .fetch_one(&db.pool).await.unwrap();

    sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();

    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM report_releases ORDER BY id")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(ids, vec![with_calendar, scheduled]);
    assert!(!ids.contains(&plain) && !ids.contains(&correction) && !ids.contains(&collider));

    let row = sqlx::query(
        "SELECT released_at, mfn_news_id, expected_date::text AS d, expected_time::text AS t, \
                calendar_label, fiscal_period, report_kind, headline, url, tags, superseded_by \
         FROM report_releases WHERE id = $1",
    )
    .bind(with_calendar)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Option<DateTime<Utc>>, _>("released_at"), None);
    assert_eq!(row.get::<Option<Uuid>, _>("mfn_news_id"), None);
    assert_eq!(row.get::<String, _>("d"), "2026-08-27");
    assert_eq!(row.get::<String, _>("t"), "08:00:00");
    assert_eq!(
        row.get::<String, _>("calendar_label"),
        "Kvartalsrapport 2026-Q2"
    );
    assert_eq!(row.get::<String, _>("fiscal_period"), "2026-Q2");
    assert_eq!(
        row.get::<Option<String>, _>("report_kind").as_deref(),
        Some("interim")
    );
    assert_eq!(row.get::<Option<String>, _>("headline"), None);
    assert_eq!(row.get::<Option<String>, _>("url"), None);
    assert!(row.get::<Vec<String>, _>("tags").is_empty());

    let sched = sqlx::query(
        "SELECT expected_date::text AS d, expected_time::text AS t, calendar_label, report_kind, \
                fiscal_period, released_at FROM report_releases WHERE id = $1",
    )
    .bind(scheduled)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(sched.get::<String, _>("d"), "2027-02-18");
    assert_eq!(sched.get::<String, _>("t"), "08:30:00");
    assert_eq!(
        sched.get::<String, _>("calendar_label"),
        "Bokslutskommuniké 2026"
    );
    assert_eq!(sched.get::<String, _>("report_kind"), "annual");
    assert_eq!(sched.get::<String, _>("fiscal_period"), "2026");

    // New shape: columns, constraints and the companies mapping column are gone/added.
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT table_name || '.' || column_name FROM information_schema.columns \
         WHERE table_name IN ('report_releases','companies') AND column_name IN \
         ('disclosure_id','category','nasdaq_news_company','mfn_news_id','lang','tags')",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let mut cols = cols;
    cols.sort();
    assert_eq!(
        cols,
        vec![
            "report_releases.lang",
            "report_releases.mfn_news_id",
            "report_releases.tags"
        ]
    );
    // CHECK: an MFN id requires a release instant.
    let bad = sqlx::query(
        "INSERT INTO report_releases (ticker, exchange_mic, mfn_news_id, expected_date) \
         VALUES ('GOMX','FNSE','00000000-0000-0000-0000-000000000001','2026-09-01')",
    )
    .execute(&db.pool)
    .await;
    assert!(bad.is_err());
    // One row per actual release.
    let insert = "INSERT INTO report_releases (ticker, exchange_mic, mfn_news_id, released_at, report_kind) \
                  VALUES ('GOMX','FNSE',$1,'2026-05-07T06:00:00Z','interim')";
    sqlx::query(insert)
        .bind(uid(1))
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(sqlx::query(insert)
        .bind(uid(2))
        .execute(&db.pool)
        .await
        .is_err());
    db.drop_db().await;
}
