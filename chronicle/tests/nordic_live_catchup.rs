//! The `nordic-live` catch-up (NEX-242) against a mocked MFN HTTP feed.
//! The watermark test needs `TEST_DATABASE_URL`; the rest always run.

mod common;

use backon::ExponentialBuilder;
use chronicle_lib::nordic::calendar::MfnClient;
use chronicle_lib::nordic::live::{
    handle_catchup_items, Companies, Disposition, Handling, Registered, SeenSet, SlackChannels,
};
use chronicle_lib::nordic::live_catchup::{
    fetch_catchup, flush, run_once, SharedWatermark, Watermark, DEFAULT_WATERMARK_AGE, MAX_PAGES,
};
use chronicle_lib::nordic::live_emit::Emitter;
use chronicle_lib::nordic::live_slack::{jobs_for_fresh, Sessions};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use common::TestDb;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn companies() -> Companies {
    Companies::new(vec![Registered {
        ticker: "GOMX".into(),
        exchange_mic: "FNSE".into(),
        mfn_slug: "gomspace-group".into(),
        name: None,
    }])
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn item(n: u128, published: DateTime<Utc>, title: &str, tags: Value) -> Value {
    json!({
        "news_id": id(n).to_string(),
        "url": format!("https://mfn.se/one/a/gomspace-group/{n}"),
        "author": {"slug": "gomspace-group", "slugs": ["gomspace-group"], "tickers": ["FNSE:GOMX"]},
        "properties": {"lang": "en", "tags": tags},
        "content": {"title": title, "publish_date": published.to_rfc3339(), "text": format!("text {n}")}
    })
}

fn news(n: u128, published: DateTime<Utc>) -> Value {
    item(
        n,
        published,
        &format!("GomSpace update {n}"),
        json!([":regulatory"]),
    )
}

async fn serve_pages(server: &MockServer, pages: &[Vec<Value>]) {
    for (n, page) in pages.iter().enumerate() {
        let next = (n + 1 < pages.len())
            .then(|| format!("{}/all/s/nordic.json?offset={}", server.uri(), n + 1));
        Mock::given(method("GET"))
            .and(path("/all/s/nordic.json"))
            .and(query_param("offset", n.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": page,
                "next_url": next,
            })))
            .mount(server)
            .await;
    }
}

fn client(server: &MockServer) -> tokio::sync::Mutex<MfnClient> {
    tokio::sync::Mutex::new(
        MfnClient::with_options(server.uri(), Duration::ZERO, "test", Duration::from_secs(5))
            .unwrap(),
    )
}

fn first_page(server: &MockServer) -> reqwest::Url {
    format!("{}/all/s/nordic.json?offset=0", server.uri())
        .parse()
        .unwrap()
}

fn fast() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(1))
        .with_max_delay(Duration::from_millis(2))
        .with_max_times(2)
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn it_stops_at_the_overlap_processes_oldest_first_and_skips_seen_items() {
    let server = MockServer::start().await;
    let now = Utc::now();
    let m = |n: i64| now - ChronoDuration::minutes(n);
    serve_pages(
        &server,
        &[
            vec![news(1, m(10)), news(2, m(20))],
            vec![news(3, m(50)), news(4, m(58))],
            vec![news(5, m(180)), news(6, m(200))],
            vec![news(7, m(300))],
        ],
    )
    .await;
    let fetched = fetch_catchup(&client(&server), first_page(&server), m(60), now, fast())
        .await
        .unwrap();
    assert_eq!(
        fetched.pages, 3,
        "page 3 reached items older than watermark - 2 min"
    );
    assert_eq!(fetched.capped, None);
    assert_eq!(requests(&server).await, 3);
    let ids: Vec<_> = fetched.items.iter().map(|i| i.news_id).collect();
    assert_eq!(
        ids,
        vec![id(4), id(3), id(2), id(1)],
        "oldest first, nothing older than the overlap"
    );

    let (emitter, mut rx) = Emitter::channel(10);
    let mut seen = SeenSet::new(100);
    assert!(seen.insert(id(3)), "item 3 already arrived on the socket");
    let handled = handle_catchup_items(
        fetched.items,
        now,
        &companies(),
        &mut seen,
        &Handling {
            emitter: Some(&emitter),
            ..Handling::default()
        },
    )
    .await;
    assert_eq!(handled.dispositions.len(), 3);
    assert_eq!(handled.counters.seen, 4);
    assert_eq!(
        handled.newest_published.unwrap().timestamp(),
        m(10).timestamp()
    );
    let mut emitted = Vec::new();
    while let Ok(job) = rx.try_recv() {
        emitted.push(job.event.source_id);
    }
    assert_eq!(
        emitted,
        vec![id(4).to_string(), id(2).to_string(), id(1).to_string()]
    );
}

#[tokio::test]
async fn it_stops_at_the_page_cap() {
    let server = MockServer::start().await;
    let now = Utc::now();
    let pages: Vec<Vec<Value>> = (0..MAX_PAGES + 10)
        .map(|n| {
            vec![news(
                n as u128 + 1,
                now - ChronoDuration::minutes(n as i64 + 1),
            )]
        })
        .collect();
    serve_pages(&server, &pages).await;
    let fetched = fetch_catchup(
        &client(&server),
        first_page(&server),
        now - ChronoDuration::days(10),
        now,
        fast(),
    )
    .await
    .unwrap();
    assert_eq!(fetched.pages, MAX_PAGES);
    assert_eq!(fetched.capped, Some("pages"));
    assert_eq!(fetched.items.len(), MAX_PAGES);
}

#[tokio::test]
async fn it_never_reads_beyond_24_hours() {
    let server = MockServer::start().await;
    let now = Utc::now();
    serve_pages(
        &server,
        &[
            vec![news(1, now - ChronoDuration::hours(1))],
            vec![
                news(2, now - ChronoDuration::hours(23)),
                news(3, now - ChronoDuration::hours(30)),
            ],
            vec![news(4, now - ChronoDuration::hours(40))],
        ],
    )
    .await;
    let fetched = fetch_catchup(
        &client(&server),
        first_page(&server),
        now - ChronoDuration::days(10),
        now,
        fast(),
    )
    .await
    .unwrap();
    assert_eq!(fetched.capped, Some("lookback"));
    assert_eq!(fetched.pages, 2);
    let ids: Vec<_> = fetched.items.iter().map(|i| i.news_id).collect();
    assert_eq!(ids, vec![id(2), id(1)]);
}

#[tokio::test]
async fn a_failing_fetch_is_retried_then_reported_and_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let watermark: SharedWatermark = Arc::new(Mutex::new(Watermark::new(Utc::now())));
    let (tx, mut rx) = mpsc::channel(4);
    run_once(
        &client(&server),
        &first_page(&server),
        &watermark,
        &tx,
        fast(),
    )
    .await;
    assert!(rx.try_recv().is_err(), "a failed run delivers nothing");
    assert_eq!(requests(&server).await, 3, "the first try and two retries");
}

#[tokio::test]
async fn a_report_older_than_the_freshness_limit_is_not_posted_to_slack() {
    let now = Utc::now();
    let report = |n: u128, age_min: i64| {
        item(
            n,
            now - ChronoDuration::minutes(age_min),
            "GomSpace Group AB: Interim report for Q3 2026",
            json!([":regulatory", "sub:report", "sub:report:interim"]),
        )
    };
    let docs = json!({"items": [report(1, 20), report(2, 5)]}).to_string();
    let mut items = chronicle_lib::nordic::feed::parse_document(&docs)
        .unwrap()
        .items;
    items.sort_by_key(|i| i.published_at);
    let mut seen = SeenSet::new(10);
    let channels = SlackChannels::new(Some("CREPORTS".into()), None);
    let h = Handling {
        channels: &channels,
        ..Handling::default()
    };
    let handled = handle_catchup_items(items, now, &companies(), &mut seen, &h).await;
    assert_eq!(handled.dispositions.len(), 2);
    assert!(handled
        .dispositions
        .iter()
        .all(|d| matches!(d, Disposition::New(_))));
    let jobs = jobs_for_fresh(
        handled.dispositions,
        &Sessions::default(),
        &channels,
        h.slack_max_age,
    );
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].item.news_id, id(2));
}

#[tokio::test]
async fn an_old_announcement_is_still_emitted() {
    let now = Utc::now();
    let docs = json!({"items": [news(1, now - ChronoDuration::minutes(20))]}).to_string();
    let items = chronicle_lib::nordic::feed::parse_document(&docs)
        .unwrap()
        .items;
    let (emitter, mut rx) = Emitter::channel(10);
    let channels = SlackChannels::new(Some("CREPORTS".into()), Some("CNEWS".into()));
    let handled = handle_catchup_items(
        items,
        now,
        &companies(),
        &mut SeenSet::new(10),
        &Handling {
            channels: &channels,
            emitter: Some(&emitter),
            ..Handling::default()
        },
    )
    .await;
    assert_eq!(handled.dispositions.len(), 1);
    assert_eq!(rx.try_recv().unwrap().event.source_id, id(1).to_string());
    let jobs = jobs_for_fresh(
        handled.dispositions,
        &Sessions::default(),
        &channels,
        Handling::default().slack_max_age,
    );
    assert!(
        jobs.is_empty(),
        "a 20-minute-old announcement is emitted but not posted"
    );
}

#[tokio::test]
async fn the_watermark_is_stored_once_and_reloaded() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let now = Utc::now();
    let fresh = Watermark::load(&db.pool, now).await.unwrap();
    assert_eq!(
        fresh.value(),
        now - DEFAULT_WATERMARK_AGE,
        "no row: 15 minutes back"
    );

    let shared: SharedWatermark = Arc::new(Mutex::new(fresh));
    flush(&db.pool, &shared, now).await;
    let newest = now - ChronoDuration::minutes(3);
    shared.lock().unwrap().observe(Some(newest));
    flush(&db.pool, &shared, now).await;
    let reloaded = Watermark::load(&db.pool, now + ChronoDuration::hours(5))
        .await
        .unwrap();
    assert_eq!(
        reloaded.value().timestamp_micros(),
        newest.timestamp_micros()
    );

    shared
        .lock()
        .unwrap()
        .observe(Some(newest - ChronoDuration::minutes(1)));
    flush(&db.pool, &shared, now).await;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM nordic_live_state")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "a single row");
    db.drop_db().await;
}
