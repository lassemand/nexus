//! The Slack fallback of the `nordic-reports` poll (NEX-223) against a mocked MFN
//! feed and a fake Slack API. Database-backed: skipped unless `TEST_DATABASE_URL`
//! is set (the smoke-test workflow sets it).

mod common;

use async_trait::async_trait;
use chronicle_lib::nordic::calendar::MfnClient;
use chronicle_lib::nordic::classify::ReportKind;
use chronicle_lib::nordic::db;
use chronicle_lib::nordic::events::EventSink;
use chronicle_lib::nordic::ingest::{process_companies_with, RunSummary};
use chronicle_lib::nordic::poll_slack::PollSlack;
use chronicle_lib::nordic::slack::{
    mark_posted, PostedRef, ReportNotice, SlackApi, SlackError, SlackMessage,
};
use chronicle_lib::nordic::NORDIC_MICS;
use chrono::{DateTime, Duration as Age, TimeZone, Utc};
use common::TestDb;
use model::generated::SpecialEvent;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NEWS_GOMSPACE: &str = include_str!("fixtures/mfn_news_gomspace.json");

/// The 2026-08-26 06:00 UTC Q2 report in the fixture.
fn q2_published() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 26, 6, 0, 0).unwrap()
}

#[derive(Default)]
struct NullSink {
    sent: Mutex<Vec<SpecialEvent>>,
}

#[async_trait]
impl EventSink for NullSink {
    async fn send(&self, _key: &str, event: &SpecialEvent) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(event.clone());
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FakeSlack {
    calls: Arc<Mutex<Vec<String>>>,
    reject_auth: bool,
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
        if self.reject_auth {
            return Err(SlackError::Api("invalid_auth".into()));
        }
        Ok(PostedRef {
            channel: "C1".into(),
            ts: "9.9".into(),
        })
    }

    async fn update(&self, at: &PostedRef, msg: &SlackMessage) -> Result<PostedRef, SlackError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("update {}: {}", at.ts, msg.text));
        Ok(at.clone())
    }
}

fn items(only_q2: bool) -> Vec<Value> {
    let all: Vec<Value> = serde_json::from_str(NEWS_GOMSPACE).unwrap();
    if only_q2 {
        all.into_iter()
            .filter(|i| i["content"]["publish_date"] == "2026-08-26T06:00:00Z")
            .collect()
    } else {
        all
    }
}

async fn serve(server: &MockServer, items: &[Value]) {
    Mock::given(method("GET"))
        .and(path("/all/a/gomspace-group.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "version": "https://www.mfn.se/feed/version/1",
            "items": items,
            "next_url": null,
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/a/gomspace-group"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html></html>"))
        .mount(server)
        .await;
}

async fn run(
    db: &TestDb,
    server: &MockServer,
    slack: &FakeSlack,
    now: DateTime<Utc>,
    backfill: bool,
) -> (RunSummary, Vec<SpecialEvent>) {
    let companies = db::load_companies(&db.pool, NORDIC_MICS).await.unwrap();
    let mut feed = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    let mut cal = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    let sink = NullSink::default();
    let insiders = common::VecInsiders::default();
    let poll = PollSlack::at(db.pool.clone(), Box::new(slack.clone()), now);
    let summary = process_companies_with(
        &db.pool,
        &mut feed,
        &mut cal,
        &sink,
        &insiders,
        &companies,
        backfill,
        Some(&poll),
    )
    .await;
    let sent = sink.sent.lock().unwrap().clone();
    (summary, sent)
}

/// A row as `nordic-live` leaves it after posting the Q2 report in `lang`.
async fn posted_by_live(db: &TestDb, lang: &str) {
    let notice = ReportNotice {
        company_name: "GomSpace".into(),
        ticker: "GOMX".into(),
        exchange_mic: "FNSE".into(),
        report_kind: ReportKind::Interim,
        fiscal_period: None,
        published_at: q2_published(),
        time_zone: chrono_tz::Europe::Stockholm,
        session: None,
        headline: String::new(),
        mfn_url: None,
        pdf_url: None,
        late: false,
        news_id: Uuid::from_u128(7),
        lang: Some(lang.into()),
    };
    let key = notice.report_key();
    sqlx::query(
        "INSERT INTO slack_report_posts (report_key, ticker, exchange_mic, news_id, lang, status, \
         claimed_at, poster, attempts) VALUES ($1, 'GOMX', 'FNSE', $2, $3, 'claimed', now(), 'live', 1)",
    )
    .bind(&key)
    .bind(notice.news_id)
    .bind(lang)
    .execute(&db.pool)
    .await
    .unwrap();
    mark_posted(&db.pool, &key, "C1", "1.1", notice.news_id, Some(lang))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_report_live_already_posted_makes_no_call() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve(&server, &items(true)).await;
    posted_by_live(&db, "en").await;
    let slack = FakeSlack::default();
    let (summary, _) = run(&db, &server, &slack, q2_published() + Age::hours(2), false).await;
    assert!(summary.is_success());
    assert!(slack.calls().is_empty());
    assert_eq!(summary.slack.posted, 0);
    db.drop_db().await;
}

#[tokio::test]
async fn a_missed_report_is_posted_once_marked_late() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve(&server, &items(true)).await;
    let slack = FakeSlack::default();
    let now = q2_published() + Age::hours(2);
    let (summary, _) = run(&db, &server, &slack, now, false).await;
    assert_eq!(summary.slack.posted, 1);
    let calls = slack.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("⏱ posted late by the daily poll (live listener missed it)"));
    assert!(calls[0].contains("26 Aug 2026 08:00 CEST"), "{}", calls[0]);
    // The next run re-reads the overlap and must not post again.
    let (again, _) = run(&db, &server, &slack, now, false).await;
    assert_eq!(again.slack.posted, 0);
    assert_eq!(slack.calls().len(), 1);
    db.drop_db().await;
}

#[tokio::test]
async fn a_report_older_than_48_hours_is_skipped_and_counted() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve(&server, &items(true)).await;
    let slack = FakeSlack::default();
    let (summary, _) = run(&db, &server, &slack, q2_published() + Age::days(3), false).await;
    assert!(slack.calls().is_empty());
    assert_eq!(summary.slack.skipped_old, 1);
    assert_eq!(summary.slack.posted, 0);
    db.drop_db().await;
}

#[tokio::test]
async fn backfill_never_posts() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve(&server, &items(true)).await;
    let slack = FakeSlack::default();
    // Fresh enough to post on a normal run.
    let (summary, sent) = run(&db, &server, &slack, q2_published() + Age::hours(1), true).await;
    assert!(!sent.is_empty(), "the backfill still publishes events");
    assert!(slack.calls().is_empty());
    assert_eq!(summary.slack, Default::default());
    db.drop_db().await;
}

#[tokio::test]
async fn a_new_company_does_not_get_its_history_posted() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve(&server, &items(false)).await;
    let slack = FakeSlack::default();
    // No cursor yet: the first incremental run reads the whole feed.
    let (summary, sent) = run(&db, &server, &slack, q2_published() + Age::hours(2), false).await;
    let releases = sent
        .iter()
        .filter(|e| e.event_type == "quarterly_report" || e.event_type == "annual_report")
        .count();
    assert!(releases > 1, "the fixture has older reports");
    assert_eq!(
        summary.slack.posted, 1,
        "only the report of the last 48 hours"
    );
    assert_eq!(summary.slack.skipped_old, releases - 1);
    assert_eq!(slack.calls().len(), 1);
    db.drop_db().await;
}

#[tokio::test]
async fn the_english_twin_replaces_a_non_english_post_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve(&server, &items(true)).await;
    posted_by_live(&db, "sv").await;
    let slack = FakeSlack::default();
    let now = q2_published() + Age::hours(2);
    let (summary, _) = run(&db, &server, &slack, now, false).await;
    assert_eq!(summary.slack.updated, 1);
    assert_eq!(summary.slack.posted, 0);
    let calls = slack.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("update 1.1: "), "{}", calls[0]);
    assert!(
        !calls[0].contains("posted late"),
        "an upgrade of the live post is not late"
    );
    let (again, _) = run(&db, &server, &slack, now, false).await;
    assert_eq!(again.slack.updated, 0);
    assert_eq!(slack.calls().len(), 1);
    db.drop_db().await;
}

#[tokio::test]
async fn a_slack_failure_does_not_fail_the_run() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = MockServer::start().await;
    serve(&server, &items(true)).await;
    let slack = FakeSlack {
        reject_auth: true,
        ..Default::default()
    };
    let (summary, sent) = run(&db, &server, &slack, q2_published() + Age::hours(2), false).await;
    assert!(summary.is_success(), "only Slack failed");
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.slack.failed, 1);
    assert!(!sent.is_empty(), "ingest and publishing carried on");
    let status: String = sqlx::query_scalar("SELECT status FROM slack_report_posts")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "failed");
    db.drop_db().await;
}
