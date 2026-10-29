//! Finnish insider transactions from MFN managers' transaction releases
//! (NEX-207), against real Metso releases. Database-backed tests are skipped
//! unless `TEST_DATABASE_URL` is set (the smoke-test workflow sets it).

mod common;

use async_trait::async_trait;
use chronicle_lib::nordic::calendar::MfnClient;
use chronicle_lib::nordic::db;
use chronicle_lib::nordic::events::EventSink;
use chronicle_lib::nordic::ingest::{process_companies, process_insiders_only, RunSummary};
use chronicle_lib::nordic::insiders::{insider_key, mar_start, InsiderSink};
use chronicle_lib::nordic::NORDIC_MICS;
use chrono::{DateTime, Utc};
use common::{TestDb, VecInsiders};
use model::generated::{unified_insider_transaction::SourceDetail, SpecialEvent};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The 18 releases of 2026-04-24T13:00Z and Sami Takaluoma's two of 2026-03-20.
const INSIDERS: &str = include_str!("fixtures/mfn/finnish/news_metso_insiders.json");
const BURST_AT: &str = "2026-04-24T13:00:00Z";

#[derive(Default)]
struct VecSink {
    sent: Mutex<Vec<SpecialEvent>>,
}

#[async_trait]
impl EventSink for VecSink {
    async fn send(&self, _key: &str, event: &SpecialEvent) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(event.clone());
        Ok(())
    }
}

fn items() -> Vec<Value> {
    serde_json::from_str(INSIDERS).unwrap()
}

async fn serve(server: &MockServer, slug: &str, items: &[Value]) {
    Mock::given(method("GET"))
        .and(path(format!("/all/a/{slug}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": items})))
        .mount(server)
        .await;
}

async fn quiet_gomspace(server: &MockServer) {
    serve(server, "gomspace-group", &[]).await;
}

async fn run(
    db: &TestDb,
    server: &MockServer,
    sink: &VecSink,
    insiders: &dyn InsiderSink,
) -> RunSummary {
    let companies = db::load_companies(&db.pool, NORDIC_MICS).await.unwrap();
    let mut feed = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    let mut cal = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    process_companies(
        &db.pool, &mut feed, &mut cal, sink, insiders, &companies, false,
    )
    .await
}

async fn insiders_only(
    db: &TestDb,
    server: &MockServer,
    insiders: &dyn InsiderSink,
    from: DateTime<Utc>,
) -> RunSummary {
    let companies = db::load_companies(&db.pool, NORDIC_MICS).await.unwrap();
    let mut feed = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    process_insiders_only(&mut feed, insiders, &companies, from).await
}

async fn cursor(db: &TestDb, ticker: &str, mic: &str) -> Option<DateTime<Utc>> {
    db::load_state(&db.pool, ticker, mic)
        .await
        .unwrap()
        .feed_cursor
}

async fn metso(db: &TestDb) {
    db.add_company_with_isin("METSO", "XHEL", "metso-2", "EUR", "FI0009014575")
        .await;
}

/// Messages as (person, transaction_date) with the MAR reference.
fn people(
    sent: &[(String, model::generated::UnifiedInsiderTransaction)],
) -> BTreeSet<(String, String)> {
    sent.iter()
        .map(|(_, m)| (m.person_name.clone(), m.transaction_date.clone()))
        .collect()
}

fn ids(
    sent: &[(String, model::generated::UnifiedInsiderTransaction)],
) -> BTreeSet<(String, String)> {
    sent.iter()
        .map(|(k, m)| {
            let Some(SourceDetail::Mar(d)) = &m.source_detail else {
                panic!("not a MAR row")
            };
            (k.clone(), d.reference_number.clone())
        })
        .collect()
}

#[tokio::test]
async fn the_burst_and_the_ceo_release_publish_ten_transactions_once_each() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    metso(&db).await;
    let server = MockServer::start().await;
    quiet_gomspace(&server).await;
    serve(&server, "metso-2", &items()).await;
    let (sink, insiders) = (VecSink::default(), VecInsiders::default());

    let summary = run(&db, &server, &sink, &insiders).await;
    assert!(summary.is_success(), "{summary:?}");
    assert_eq!(summary.insiders_published, 10);
    assert_eq!(
        (summary.mar_parse_errors, summary.mar_suspicious_zero_price),
        (0, 0)
    );
    // Managers' transactions are not reports.
    assert!(sink.sent.lock().unwrap().is_empty());

    let sent = insiders.take();
    assert_eq!(sent.len(), 10);
    let april: Vec<_> = sent
        .iter()
        .filter(|(_, m)| m.published_date == "2026-04-24")
        .collect();
    assert_eq!(april.len(), 9, "18 releases are 9 people's transactions");
    assert_eq!(
        april
            .iter()
            .map(|(_, m)| m.person_name.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        9
    );
    for (key, m) in &sent {
        assert_eq!(key, &insider_key(m));
        assert_eq!(
            (
                m.ticker.as_str(),
                m.exchange_mic.as_str(),
                m.currency.as_str()
            ),
            ("METSO", "XHEL", "EUR")
        );
        assert_eq!(
            m.source_registry,
            model::generated::SourceRegistry::FinMar as i32
        );
        assert_ne!(
            m.transaction_type,
            model::generated::UnifiedTransactionType::Buy as i32,
            "an incentive is never a buy"
        );
    }
    let ceo = sent
        .iter()
        .find(|(_, m)| m.person_name == "Sami Takaluoma")
        .unwrap();
    assert_eq!(
        (
            ceo.1.transaction_date.as_str(),
            ceo.1.published_date.as_str(),
            ceo.1.volume,
            ceo.1.price_per_unit
        ),
        ("2026-03-19", "2026-03-20", 23201.0, 0.0)
    );
    // The cursor moved to the newest release read.
    assert_eq!(
        cursor(&db, "METSO", "XHEL").await.unwrap().to_rfc3339(),
        "2026-04-24T13:00:00+00:00"
    );
    db.drop_db().await;
}

/// The same releases under a Swedish and a Danish company publish nothing:
/// Swedish insiders come from FI's register, Danish ones are out of scope.
#[tokio::test]
async fn swedish_and_danish_companies_publish_no_insiders_from_mfn() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.add_company("VOLV B", "XSTO", "volvo").await;
    db.add_company_with_isin("NOVO-B", "XCSE", "novo-nordisk", "DKK", "DK0062498333")
        .await;
    let as_author = |slug: &str, ticker: &str, isins: Vec<&str>| -> Vec<Value> {
        items()
            .into_iter()
            .map(|mut i| {
                i["author"] =
                    json!({"slug": slug, "slugs": [slug], "tickers": [ticker], "isins": isins});
                i
            })
            .collect()
    };
    let server = MockServer::start().await;
    quiet_gomspace(&server).await;
    serve(&server, "volvo", &as_author("volvo", "XSTO:VOLV B", vec![])).await;
    serve(
        &server,
        "novo-nordisk",
        &as_author("novo-nordisk", "XCSE:NOVO B", vec!["DK0062498333"]),
    )
    .await;
    let (sink, insiders) = (VecSink::default(), VecInsiders::default());

    let summary = run(&db, &server, &sink, &insiders).await;
    assert!(summary.is_success(), "{summary:?}");
    assert!(insiders.take().is_empty());
    assert_eq!(summary.insiders_published, 0);
    assert!(
        sink.sent.lock().unwrap().is_empty(),
        "and no report events either"
    );

    // The one-off re-read leaves them alone as well.
    let only = VecInsiders::default();
    let summary = insiders_only(
        &db,
        &server,
        &only,
        mar_start().and_hms_opt(0, 0, 0).unwrap().and_utc(),
    )
    .await;
    assert_eq!(summary.attempted, 0, "no Finnish company registered");
    assert!(only.take().is_empty());
    db.drop_db().await;
}

#[tokio::test]
async fn a_failed_insider_publish_leaves_the_cursor_and_the_next_run_resends() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    metso(&db).await;
    let server = MockServer::start().await;
    quiet_gomspace(&server).await;
    serve(&server, "metso-2", &items()).await;
    let sink = VecSink::default();

    let flaky = VecInsiders::failing_after(3);
    let summary = run(&db, &server, &sink, &flaky).await;
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert_eq!(
        flaky.take().len(),
        3,
        "three went out before the broker failed"
    );
    assert_eq!(
        cursor(&db, "METSO", "XHEL").await,
        None,
        "the cursor did not move"
    );

    let healthy = VecInsiders::default();
    let summary = run(&db, &server, &sink, &healthy).await;
    assert!(summary.is_success(), "{summary:?}");
    let sent = healthy.take();
    assert_eq!(sent.len(), 10, "everything is read again and re-published");
    assert_eq!(people(&sent).len(), 10);
    assert!(cursor(&db, "METSO", "XHEL").await.is_some());
    db.drop_db().await;
}

#[tokio::test]
async fn an_unparsable_release_is_counted_and_does_not_hold_the_cursor() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    metso(&db).await;
    let mut all = items();
    // A newer managers' transaction whose text is not a notification.
    let mut broken = all[0].clone();
    broken["news_id"] = json!("22222222-2222-4222-8222-222222222222");
    broken["content"]["publish_date"] = json!("2026-05-04T10:00:00Z");
    broken["content"]["title"] = json!("Metso Corporation: Managers' transactions - Nobody");
    broken["properties"]["lang"] = json!("en");
    broken["content"]["text"] = json!("Ilmoitusvelvollinen\nnothing else");
    all.insert(0, broken);
    let server = MockServer::start().await;
    quiet_gomspace(&server).await;
    serve(&server, "metso-2", &all).await;
    let (sink, insiders) = (VecSink::default(), VecInsiders::default());

    let summary = run(&db, &server, &sink, &insiders).await;
    assert!(summary.is_success(), "{summary:?}");
    assert_eq!(
        (summary.mar_parse_errors, summary.mar_suspicious_zero_price),
        (1, 0)
    );
    assert_eq!(insiders.take().len(), 10, "the rest is published");
    assert_eq!(
        cursor(&db, "METSO", "XHEL").await.unwrap().to_rfc3339(),
        "2026-05-04T10:00:00+00:00"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn the_insiders_only_reread_publishes_no_reports_and_never_moves_the_cursor() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    metso(&db).await;
    let server = MockServer::start().await;
    quiet_gomspace(&server).await;
    serve(&server, "metso-2", &items()).await;
    let sink = VecSink::default();
    let first = VecInsiders::default();
    run(&db, &server, &sink, &first).await;
    let before = cursor(&db, "METSO", "XHEL").await;
    assert!(before.is_some());

    // Everything since MAR started: the same ten, even though the cursor is past them.
    let again = VecInsiders::default();
    let summary = insiders_only(
        &db,
        &server,
        &again,
        mar_start().and_hms_opt(0, 0, 0).unwrap().and_utc(),
    )
    .await;
    assert!(summary.is_success(), "{summary:?}");
    assert_eq!((summary.attempted, summary.insiders_published), (1, 10));
    assert_eq!(
        ids(&again.take()),
        ids(&first.take()),
        "same keys and references: idempotent downstream"
    );
    assert_eq!(
        cursor(&db, "METSO", "XHEL").await,
        before,
        "the cursor did not move"
    );
    assert!(sink.sent.lock().unwrap().is_empty());

    // From a date: only the April burst.
    let april = VecInsiders::default();
    let from = "2026-04-01T00:00:00Z".parse().unwrap();
    let summary = insiders_only(&db, &server, &april, from).await;
    assert_eq!(summary.insiders_published, 9);
    assert!(april
        .take()
        .iter()
        .all(|(_, m)| m.published_date == "2026-04-24"));
    assert_eq!(BURST_AT, "2026-04-24T13:00:00Z");
    db.drop_db().await;
}

#[tokio::test]
async fn a_wrong_company_publishes_no_insiders_either() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // Metso's ticker and ISIN on the slug of the old Metso / Neles.
    db.add_company_with_isin("METSO", "XHEL", "metso", "EUR", "FI0009014575")
        .await;
    let neles = |mut i: Value| {
        i["author"] = json!({"slug": "neles", "slugs": ["metso", "metso-corporation", "neles"],
                             "tickers": ["XHEL:NELES"], "isins": ["FI4000440664"]});
        i
    };
    let wrong: Vec<Value> = items().into_iter().map(neles).collect();
    let server = MockServer::start().await;
    quiet_gomspace(&server).await;
    serve(&server, "metso", &wrong).await;
    let (sink, insiders) = (VecSink::default(), VecInsiders::default());

    assert_eq!(run(&db, &server, &sink, &insiders).await.failed, 1);
    let only = insiders_only(
        &db,
        &server,
        &insiders,
        mar_start().and_hms_opt(0, 0, 0).unwrap().and_utc(),
    )
    .await;
    assert_eq!(only.failed, 1);
    assert!(insiders.take().is_empty());
    db.drop_db().await;
}
