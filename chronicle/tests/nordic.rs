//! Database-backed integration tests for the Nordic report tables. Skipped
//! unless `TEST_DATABASE_URL` is set (the smoke-test workflow sets it).

mod common;

use chronicle_lib::nordic::calendar::{
    apply_calendar, classify_label, refresh_company, CalendarEntry, LabelClass, MfnClient,
    ParseError, Refresh,
};
use chronicle_lib::nordic::db::{self, record_release, NewRelease, Recorded};
use chronicle_lib::nordic::feed::NewsClient;
use chronicle_lib::nordic::ingest::{ingest_company, process_companies, CompanyOutcome};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use common::TestDb;
use sqlx::{PgPool, Row};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NASDAQ_FIXTURE: &str = include_str!("fixtures/nasdaq_news_gomspace.json");
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

fn release(disclosure_id: i64, released_at: DateTime<Utc>) -> NewRelease<'static> {
    NewRelease {
        ticker: "GOMX",
        exchange_mic: "FNSE",
        disclosure_id,
        released_at,
        category: "Interim report (Q1 and Q3)",
        report_kind: Some("interim"),
        headline: "Q2 report",
        url: Some("https://example.test/x"),
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
        "SELECT disclosure_id, released_at, expected_date::text AS d, \
                    expected_time::text AS t, calendar_label, fiscal_period, report_kind, headline \
             FROM report_releases WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Option<i64>, _>("disclosure_id"), Some(10));
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
        sqlx::query_scalar("SELECT id FROM report_releases WHERE disclosure_id = 12")
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
    let matched: i64 =
        sqlx::query_scalar("SELECT id FROM report_releases WHERE disclosure_id = 13")
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
    // disclosure_id without released_at.
    let disclosure_only = sqlx::query(
        "INSERT INTO report_releases (ticker, exchange_mic, disclosure_id, expected_date) \
             VALUES ('GOMX','FNSE',99,'2026-08-27')",
    )
    .execute(&db.pool)
    .await;
    assert!(disclosure_only.is_err());
    // released_at without disclosure_id.
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

// ── Fixture classification (no database) ────────────────────────────────────

mod fixture {
    use chronicle_lib::nordic::classify;
    use chronicle_lib::nordic::feed::NewsItem;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Fixture {
        results: R,
    }
    #[derive(Deserialize)]
    struct R {
        item: Vec<NewsItem>,
    }

    fn items() -> Vec<NewsItem> {
        serde_json::from_str::<Fixture>(super::NASDAQ_FIXTURE)
            .unwrap()
            .results
            .item
    }

    fn kind(id: i64) -> Option<&'static str> {
        let items = items();
        let i = items.iter().find(|i| i.disclosure_id == id).unwrap();
        classify::report_kind(&i.cns_category, &i.headline).map(|k| k.as_str())
    }

    #[test]
    fn gomspace_fixture_classification() {
        assert_eq!(kind(1460208), Some("interim"));
        assert_eq!(kind(1440487), Some("interim"));
        assert_eq!(kind(1430824), None); // order intake news
        assert_eq!(kind(1413953), None); // market guidance
        assert_eq!(kind(1421253), Some("annual"));
        assert_eq!(kind(1385457), Some("interim"));
        assert_eq!(kind(1385496), None); // its correction
        assert_eq!(kind(1373582), None); // non-report correction
        assert_eq!(kind(1312423), Some("interim")); // older category
        assert_eq!(kind(1307899), None); // market update
        assert_eq!(kind(1464054), None);
    }

    #[test]
    fn gomspace_fixture_flags_and_utc_times() {
        let items = items();
        let get = |id| items.iter().find(|i| i.disclosure_id == id).unwrap();
        assert!(classify::is_correction(&get(1385496).headline));
        assert!(classify::is_correction(&get(1373582).headline));
        assert!(classify::is_calendar_update(&get(1340291).headline));
        assert!(classify::is_calendar_update(&get(1215246).headline));
        assert!(!classify::is_calendar_update(&get(1464054).headline));
        assert_eq!(
            get(1460208).released_at().unwrap().to_rfc3339(),
            "2026-08-26T06:00:00+00:00"
        );
        assert_eq!(
            get(1421253).released_at().unwrap().to_rfc3339(),
            "2026-02-19T07:00:25+00:00"
        );
    }
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
        "SELECT disclosure_id, expected_date::text AS d, calendar_label, fiscal_period \
         FROM report_releases",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Option<i64>, _>("disclosure_id"), Some(40));
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
            "SELECT id, disclosure_id, released_at, expected_date, expected_time, calendar_label, \
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
                r.get::<Option<i64>, _>("disclosure_id"),
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

// ── nordic-reports run (database) ───────────────────────────────────────────

#[tokio::test]
async fn ingest_is_idempotent_and_links_corrections() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(NASDAQ_FIXTURE))
        .mount(&server)
        .await;
    let mut client =
        NewsClient::new(server.uri(), Duration::ZERO, Duration::from_millis(1)).unwrap();
    let company = &db::load_companies(&db.pool, &["FNSE"]).await.unwrap()[0];

    let first = ingest_company(&db.pool, &mut client, company, "GomSpace Group AB")
        .await
        .unwrap();
    assert_eq!(first.inserted, 17);
    assert_eq!(first.new_calendar_disclosures.len(), 2);
    let second = ingest_company(&db.pool, &mut client, company, "GomSpace Group AB")
        .await
        .unwrap();
    assert_eq!(second, CompanyOutcome::default());

    let counted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM report_releases WHERE report_kind IS NOT NULL AND NOT is_correction",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(counted, 9);
    let row = sqlx::query(
        "SELECT o.superseded_by, c.id AS cid, c.report_kind, c.is_correction \
         FROM report_releases o, report_releases c \
         WHERE o.disclosure_id = 1385457 AND c.disclosure_id = 1385496",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        row.get::<Option<i64>, _>("superseded_by"),
        Some(row.get("cid"))
    );
    assert_eq!(row.get::<Option<String>, _>("report_kind"), None);
    assert!(row.get::<bool, _>("is_correction"));
    db.drop_db().await;
}

#[tokio::test]
async fn calendar_announcement_triggers_exactly_one_mfn_fetch() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let nasdaq = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(NASDAQ_FIXTURE))
        .mount(&nasdaq)
        .await;
    let mfn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a/gomspace-group"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MFN_FIXTURE))
        .mount(&mfn)
        .await;

    let mut news = NewsClient::new(nasdaq.uri(), Duration::ZERO, Duration::from_millis(1)).unwrap();
    let mut client = MfnClient::new(mfn.uri(), Duration::ZERO).unwrap();
    let companies = db::load_companies(&db.pool, &["FNSE", "XSTO"])
        .await
        .unwrap();

    // The feed holds two new "updates its financial calendar" items → one fetch.
    let first = process_companies(&db.pool, &mut news, &mut client, &companies).await;
    assert_eq!((first.attempted, first.failed), (1, 0));
    assert_eq!(first.calendars_refreshed, vec!["GOMX"]);
    assert_eq!(mfn.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        scheduled_labels(&db.pool).await,
        vec!["Bokslutskommuniké 2026"]
    );

    // Same feed again: nothing newly inserted, so no MFN request.
    let second = process_companies(&db.pool, &mut news, &mut client, &companies).await;
    assert!(second.calendars_refreshed.is_empty());
    assert_eq!(mfn.received_requests().await.unwrap().len(), 1);

    // A failing refresh doesn't fail the run.
    sqlx::query("DELETE FROM report_releases")
        .execute(&db.pool)
        .await
        .unwrap();
    mfn.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mfn)
        .await;
    let third = process_companies(&db.pool, &mut news, &mut client, &companies).await;
    assert!(third.is_success());
    assert!(third.calendars_refreshed.is_empty());
    db.drop_db().await;
}
