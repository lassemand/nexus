//! Finnish issuers on the Nordic report jobs (NEX-204), against real MFN items
//! for Metso (`metso-2`) and the Neles/old-Metso entities that share its name.
//! Database-backed tests are skipped unless `TEST_DATABASE_URL` is set.

mod common;

use async_trait::async_trait;
use chronicle_lib::nordic::calendar::{parse_calendar, MfnClient};
use chronicle_lib::nordic::classify::{classify_item, Classification, ReportKind};
use chronicle_lib::nordic::db::{self, Company};
use chronicle_lib::nordic::events::{release_event, EventSink};
use chronicle_lib::nordic::feed::{parse_document, NewsItem};
use chronicle_lib::nordic::ingest::process_companies;
use chronicle_lib::nordic::NORDIC_MICS;
use common::TestDb;
use model::generated::SpecialEvent;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NEWS_METSO: &str = include_str!("fixtures/mfn/finnish/news_metso.json");
const NEWS_NELES: &str = include_str!("fixtures/mfn/finnish/news_neles.json");
const CALENDAR_METSO: &str = include_str!("fixtures/mfn/finnish/mfn_metso.html");

/// 2026-04-24T13:00:00Z: nine people, each in Finnish and English.
/// Words of the report titles in the fixtures, to pick them out of a day's items.
const REPORT_WORDS: &[&str] = &[
    "osavuosikatsaus",
    "puolivuosikatsaus",
    "tilinpäätöstiedote",
    "interim report",
    "interim review",
    "half-year report",
    "half-year review",
    "financial statements review",
    "half year financial report",
    "vuosikertomus",
    "annual report",
];

const BURST_AT: &str = "2026-04-24T13:00:00Z";

fn items_of(fixture: &str) -> Vec<NewsItem> {
    let doc = parse_document(&format!(r#"{{"items": {fixture}}}"#)).unwrap();
    assert!(doc.bad_items.is_empty(), "{:?}", doc.bad_items);
    doc.items
}

fn metso_company() -> Company {
    Company {
        ticker: "METSO".into(),
        exchange_mic: "XHEL".into(),
        mfn_slug: Some("metso-2".into()),
        name: Some("Metso".into()),
        isin: Some("FI0009014575".into()),
    }
}

fn classify(i: &NewsItem) -> Classification {
    classify_item(&i.tags, &i.title)
}

fn on<'a>(items: &'a [NewsItem], date: &str, lang: &str) -> Vec<&'a NewsItem> {
    items
        .iter()
        .filter(|i| {
            i.published_at.to_rfc3339().starts_with(date) && i.lang.as_deref() == Some(lang)
        })
        .collect()
}

// ── Classification of real titles (no database) ─────────────────────────────

#[test]
fn metso_reports_in_both_languages() {
    let items = items_of(NEWS_METSO);
    // (date, expected kind, expected fiscal period) — fi and en alike.
    let expected = [
        ("2026-10-22", Some(ReportKind::Interim), Some("2026-Q3")),
        ("2026-07-24", Some(ReportKind::Interim), Some("2026-Q2")),
        ("2026-04-22", Some(ReportKind::Interim), Some("2026-Q1")),
        // Tagged only `:regulatory`: the headline words decide.
        ("2026-02-12", Some(ReportKind::Annual), Some("2025")),
        ("2025-10-23", Some(ReportKind::Interim), Some("2025-Q3")),
        // Outotec / Metso Outotec era, same ISIN.
        ("2019-07-26", Some(ReportKind::Interim), Some("2019-Q2")),
        ("2020-10-28", Some(ReportKind::Interim), Some("2020-Q3")),
        // The annual report *document* is not a results release.
        ("2026-03-04", None, None),
    ];
    for (date, kind, period) in expected {
        for lang in ["fi", "en"] {
            // The day's other releases (flagging notices, ...) are not reports.
            let found: Vec<_> = on(&items, date, lang)
                .into_iter()
                .filter(|i| {
                    REPORT_WORDS
                        .iter()
                        .any(|w| i.title.to_lowercase().contains(w))
                })
                .collect();
            assert!(!found.is_empty(), "no {lang} report item on {date}");
            for item in found {
                let c = classify(item);
                assert_eq!(c.report_kind, kind, "{date} {lang}: {}", item.title);
                assert_eq!(
                    c.fiscal_period.as_deref(),
                    period,
                    "{date} {lang}: {}",
                    item.title
                );
                assert!(!c.is_correction && !c.calendar_update, "{}", item.title);
            }
        }
    }
}

#[test]
fn the_finnish_and_english_twin_of_every_report_classify_alike() {
    let items = items_of(NEWS_METSO);
    let mut by_instant: HashMap<String, Vec<(&NewsItem, Classification)>> = HashMap::new();
    for i in &items {
        let c = classify(i);
        if c.report_kind.is_some() {
            by_instant
                .entry(i.published_at.to_rfc3339())
                .or_default()
                .push((i, c));
        }
    }
    assert!(
        by_instant.len() >= 6,
        "{} report instants",
        by_instant.len()
    );
    for (at, group) in by_instant {
        let langs: BTreeSet<_> = group.iter().filter_map(|(i, _)| i.lang.clone()).collect();
        assert_eq!(langs.len(), 2, "{at}: a fi and an en copy, got {langs:?}");
        let (first, rest) = group.split_first().unwrap();
        for (i, c) in rest {
            assert_eq!(
                (c.report_kind, &c.fiscal_period),
                (first.1.report_kind, &first.1.fiscal_period),
                "{at}: {} vs {}",
                i.title,
                first.0.title
            );
        }
        // Different MFN ids and groups: nothing but kind, instant and company links them.
        assert_ne!(group[0].0.news_id, group[1].0.news_id);
    }
}

#[test]
fn the_burst_of_managers_transactions_produces_no_events() {
    let items = items_of(NEWS_METSO);
    let burst: Vec<_> = items
        .iter()
        .filter(|i| i.published_at.to_rfc3339() == "2026-04-24T13:00:00+00:00")
        .collect();
    assert_eq!(burst.len(), 18, "9 people x fi/en");
    let company = metso_company();
    for item in burst {
        let c = classify(item);
        assert_eq!(c.report_kind, None, "{}", item.title);
        assert!(!c.is_correction && !c.calendar_update, "{}", item.title);
        assert!(
            release_event(&company, item, &c).is_none(),
            "{}",
            item.title
        );
    }
}

#[test]
fn notices_that_only_mention_reports_are_not_published() {
    let items = items_of(NEWS_METSO);
    let company = metso_company();
    let published: Vec<_> = items
        .iter()
        .filter(|i| release_event(&company, i, &classify(i)).is_some())
        .collect();
    for i in &published {
        let t = i.title.to_lowercase();
        assert!(
            !t.contains("yhtiökokou")
                && !t.contains("kutsu")
                && !t.contains("buyback")
                && !t.contains("omien osakkeiden")
                && !t.contains("restated")
                && !t.contains("oikaist")
                && !t.contains("vuosikertomus")
                && !t.contains("annual report"),
            "{}",
            i.title
        );
    }
    assert!(!published.is_empty());
}

#[test]
fn neles_oddity_and_calendar_notices() {
    let items = items_of(NEWS_NELES);
    let fy: Vec<_> = on(&items, "2022-02-04", "fi")
        .into_iter()
        .filter(|i| i.title.contains("osavuosikatsaus"))
        .collect();
    assert_eq!(fy.len(), 1);
    // Titled "osavuosikatsaus" but for January-December: the period beats the word.
    let c = classify(fy[0]);
    assert_eq!(
        (c.report_kind, c.fiscal_period.as_deref()),
        (Some(ReportKind::Annual), Some("2021"))
    );
    for lang in ["fi", "en"] {
        let notices: Vec<_> = on(&items, "2022-01-14", lang)
            .into_iter()
            .filter(|i| i.title.contains("julkaisee") || i.title.contains("to publish"))
            .collect();
        assert_eq!(notices.len(), 1, "{lang}");
        for i in notices {
            let c = classify(i);
            assert!(c.calendar_update, "{}", i.title);
            assert_eq!(c.report_kind, None, "{}", i.title);
        }
    }
}

// ── Calendar (no database) ──────────────────────────────────────────────────

#[test]
fn metso_calendar_page_parses_and_ignores_non_reports() {
    let entries = parse_calendar(CALENDAR_METSO, "metso-2").unwrap();
    let q3 = entries
        .iter()
        .find(|e| e.date.to_string() == "2027-10-25")
        .expect("Q3 2027 row");
    assert_eq!(q3.time.map(|t| t.to_string()).as_deref(), Some("08:00:00"));
    assert_eq!(q3.kind, ReportKind::Interim);
    assert_eq!(q3.fiscal_period.as_deref(), Some("2027-Q3"));
    let fy = entries
        .iter()
        .find(|e| e.date.to_string() == "2027-02-19")
        .unwrap();
    assert_eq!(
        (fy.kind, fy.fiscal_period.as_deref()),
        (ReportKind::Annual, Some("2026"))
    );
    // The AGM and the ex-dividend days are not reports.
    assert!(entries
        .iter()
        .all(|e| !e.label.to_lowercase().contains("årsstämma")));
    assert!(entries
        .iter()
        .all(|e| !e.label.to_lowercase().contains("x-dag")));
}

// ── Publishing against a mocked MFN feed (database for the state table) ────

#[derive(Default)]
struct VecSink {
    sent: Mutex<Vec<(String, SpecialEvent)>>,
}

#[async_trait]
impl EventSink for VecSink {
    async fn send(&self, key: &str, event: &SpecialEvent) -> anyhow::Result<()> {
        self.sent
            .lock()
            .unwrap()
            .push((key.to_string(), event.clone()));
        Ok(())
    }
}

async fn serve(server: &MockServer, slug: &str, fixture: &str) {
    let items: Value = serde_json::from_str(fixture).unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/all/a/{slug}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": items})))
        .mount(server)
        .await;
}

async fn run(
    db: &TestDb,
    server: &MockServer,
    sink: &VecSink,
) -> chronicle_lib::nordic::ingest::RunSummary {
    let companies = db::load_companies(&db.pool, NORDIC_MICS).await.unwrap();
    assert!(
        companies.iter().any(|c| c.exchange_mic == "XHEL"),
        "NORDIC_MICS includes Helsinki"
    );
    let mut feed = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    let mut cal = MfnClient::new(server.uri(), Duration::ZERO).unwrap();
    let insiders = common::VecInsiders::default();
    process_companies(
        &db.pool, &mut feed, &mut cal, sink, &insiders, &companies, false,
    )
    .await
}

async fn quiet_gomspace(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/all/a/gomspace-group.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(server)
        .await;
}

#[tokio::test]
async fn metso_publishes_exactly_its_reports_including_the_earlier_names() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.add_company_with_isin("METSO", "XHEL", "metso-2", "EUR", "FI0009014575")
        .await;
    let server = MockServer::start().await;
    quiet_gomspace(&server).await;
    serve(&server, "metso-2", NEWS_METSO).await;
    let sink = VecSink::default();

    let summary = run(&db, &server, &sink).await;
    assert!(summary.is_success(), "{summary:?}");
    let sent = sink.sent.lock().unwrap().clone();
    assert!(sent
        .iter()
        .all(|(k, e)| k == "METSO:XHEL" && e.exchange_mic == "XHEL"));
    let periods: BTreeSet<(String, String, String)> = sent
        .iter()
        .map(|(_, e)| {
            (
                e.event_type.clone(),
                e.fiscal_period.clone(),
                e.lang.clone(),
            )
        })
        .collect();
    for (ty, period) in [
        ("quarterly_report", "2026-Q3"),
        ("quarterly_report", "2026-Q2"),
        ("quarterly_report", "2026-Q1"),
        ("annual_report", "2025"),
        ("quarterly_report", "2025-Q3"),
        // Published under the `outotec` / `metso-outotec` entities: the ISIN ties them.
        ("quarterly_report", "2019-Q2"),
        ("quarterly_report", "2020-Q3"),
    ] {
        for lang in ["fi", "en"] {
            assert!(
                periods.contains(&(ty.into(), period.into(), lang.into())),
                "missing {ty} {period} {lang}: {periods:?}"
            );
        }
    }
    // No managers' transactions, no annual report document, nothing from Neles.
    let burst_ids: BTreeSet<String> = items_of(NEWS_METSO)
        .iter()
        .filter(|i| i.published_at.to_rfc3339().starts_with(&BURST_AT[..16]))
        .map(|i| i.news_id.to_string())
        .collect();
    assert_eq!(burst_ids.len(), 18);
    assert!(sent.iter().all(|(_, e)| !burst_ids.contains(&e.source_id)));
    assert!(sent
        .iter()
        .all(|(_, e)| !e.description.to_lowercase().contains("vuosikertomus")));
    assert!(sent
        .iter()
        .all(|(_, e)| !e.description.to_lowercase().contains("neles")));
    db.drop_db().await;
}

#[tokio::test]
async fn a_registry_row_on_the_neles_slug_publishes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // Misconfigured: Metso's ticker and ISIN, but the slug of the old Metso / Neles.
    db.add_company_with_isin("METSO", "XHEL", "metso", "EUR", "FI0009014575")
        .await;
    let server = MockServer::start().await;
    quiet_gomspace(&server).await;
    serve(&server, "metso", NEWS_NELES).await;
    let sink = VecSink::default();

    let summary = run(&db, &server, &sink).await;
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert!(
        sink.sent.lock().unwrap().is_empty(),
        "no Neles release under METSO"
    );
    db.drop_db().await;
}
