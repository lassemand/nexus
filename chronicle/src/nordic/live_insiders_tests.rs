//! The live insider path (NEX-279): a Finnish managers' transaction read by
//! `nordic-live` produces the messages `nordic-reports` publishes for it.
//!
//! Fixtures are the Metso releases of `insiders.rs` (`news_metso_insiders.json`)
//! and the Wärtsilä close associate pair of `mar_release.rs`.

use super::db::Company;
use super::events::trace_headers;
use super::feed::{parse_document, NewsItem};
use super::insiders::{self, insider_key, InsiderSink};
use super::live::{
    handle_catchup_items, handle_message_full, Companies, Handling, Registered, SeenSet,
};
use super::live_insiders::{
    parse_release, run_insider_producer, InsiderQueue, INSIDER_LATENCY_BUCKETS,
};
use async_trait::async_trait;
use backon::ExponentialBuilder;
use chrono::Utc;
use model::generated::{unified_insider_transaction::SourceDetail, UnifiedInsiderTransaction};
use rdkafka::message::Headers;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const INSIDERS: &str = include_str!("../../tests/fixtures/mfn/finnish/news_metso_insiders.json");
const WARTSILA_EN: &str =
    include_str!("../../tests/fixtures/mfn/finnish/mar/wartsila_close_associate_en.txt");
const WARTSILA_FI: &str =
    include_str!("../../tests/fixtures/mfn/finnish/mar/wartsila_close_associate_fi.txt");

const CEO_FI: &str = "0d603af2-5858-58ee-a4e4-821966dc8200";
const CEO_EN: &str = "9954de80-88dd-5084-a03b-4b42e17c1644";

fn raw_items() -> Vec<serde_json::Value> {
    serde_json::from_str(INSIDERS).unwrap()
}

fn raw(news_id: &str) -> serde_json::Value {
    raw_items()
        .into_iter()
        .find(|i| i["news_id"] == news_id)
        .unwrap()
}

/// One socket message carrying `items`.
fn message(items: &[serde_json::Value]) -> String {
    serde_json::json!({ "items": items }).to_string()
}

fn news(item: &serde_json::Value) -> NewsItem {
    parse_document(&message(std::slice::from_ref(item)))
        .unwrap()
        .items
        .remove(0)
}

/// The CEO release with another text, language and id.
fn variant(news_id: &str, lang: &str, text: &str) -> serde_json::Value {
    let mut item = raw(CEO_EN);
    item["news_id"] = serde_json::json!(news_id);
    item["properties"]["lang"] = serde_json::json!(lang);
    item["content"]["text"] = serde_json::json!(text);
    item
}

fn metso_on(mic: &str) -> Companies {
    Companies::new(vec![Registered {
        ticker: "METSO".into(),
        exchange_mic: mic.into(),
        mfn_slug: "metso-2".into(),
        name: Some("Metso".into()),
    }])
}

fn metso() -> Company {
    Company {
        ticker: "METSO".into(),
        exchange_mic: "XHEL".into(),
        mfn_slug: Some("metso-2".into()),
        name: Some("Metso".into()),
        isin: None,
    }
}

type Sent = Vec<(String, UnifiedInsiderTransaction, Vec<(String, String)>)>;

#[derive(Default)]
struct VecSink {
    sent: Mutex<Sent>,
}

#[async_trait]
impl InsiderSink for VecSink {
    async fn send(&self, key: &str, txn: &UnifiedInsiderTransaction) -> anyhow::Result<()> {
        let headers = trace_headers()
            .iter()
            .map(|h| {
                (
                    h.key.to_string(),
                    String::from_utf8(h.value.unwrap_or_default().to_vec()).unwrap(),
                )
            })
            .collect();
        self.sent
            .lock()
            .unwrap()
            .push((key.to_string(), txn.clone(), headers));
        Ok(())
    }
}

fn fast() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(1))
        .with_max_delay(Duration::from_millis(2))
        .with_total_delay(Some(Duration::from_millis(10)))
        .without_max_times()
}

/// Reads `messages` live, one per socket message, and returns what reached
/// `insider.transactions`.
async fn live(companies: &Companies, messages: &[String]) -> Sent {
    let (queue, jobs) = InsiderQueue::channel(64);
    let handling = Handling {
        insiders: Some(&queue),
        ..Handling::default()
    };
    let mut seen = SeenSet::new(100);
    for m in messages {
        handle_message_full(m, Utc::now(), companies, &mut seen, &handling).await;
    }
    drop(queue);
    let sink = Arc::new(VecSink::default());
    run_insider_producer(sink.clone(), jobs, fast()).await;
    let sent = std::mem::take(&mut *sink.sent.lock().unwrap());
    sent
}

fn keyed(m: &Sent) -> Vec<(String, UnifiedInsiderTransaction)> {
    m.iter().map(|(k, t, _)| (k.clone(), t.clone())).collect()
}

fn poll(items: &[NewsItem]) -> Vec<(String, UnifiedInsiderTransaction)> {
    let refs: Vec<&NewsItem> = items.iter().collect();
    insiders::build(&metso(), &refs)
        .messages
        .into_iter()
        .map(|(_, m)| (insider_key(&m), m))
        .collect()
}

/// What signal's unique key and the Kafka key are made of, plus the fields the
/// task requires to agree between the two languages.
#[derive(Debug, PartialEq)]
struct Identity {
    key: String,
    ticker: String,
    person: String,
    transaction_date: String,
    published_date: String,
    volume: f64,
    price: f64,
    kind: i32,
    currency: String,
    reference: String,
}

fn identity((key, m): &(String, UnifiedInsiderTransaction)) -> Identity {
    let reference = match &m.source_detail {
        Some(SourceDetail::Mar(d)) => d.reference_number.clone(),
        other => panic!("expected MAR detail, got {other:?}"),
    };
    Identity {
        key: key.clone(),
        ticker: m.ticker.clone(),
        person: m.person_name.clone(),
        transaction_date: m.transaction_date.clone(),
        published_date: m.published_date.clone(),
        volume: m.volume,
        price: m.price_per_unit,
        kind: m.transaction_type,
        currency: m.currency.clone(),
        reference,
    }
}

fn lang((_, m): &(String, UnifiedInsiderTransaction)) -> String {
    match &m.source_detail {
        Some(SourceDetail::Mar(d)) => d.lang.clone(),
        _ => String::new(),
    }
}

#[tokio::test]
async fn a_finnish_managers_transaction_read_live_is_what_the_poll_publishes() {
    let sent = live(&metso_on("XHEL"), &[message(&[raw(CEO_EN)])]).await;
    let expected = poll(&[news(&raw(CEO_EN))]);
    assert_eq!(expected.len(), 1);
    assert_eq!(keyed(&sent), expected);
    assert_eq!(sent[0].0, "FIN_MAR:METSO:SAMI TAKALUOMA:2026-03-19");
    assert_eq!(sent[0].0, insider_key(&sent[0].1));
}

#[tokio::test]
async fn the_item_is_still_an_announcement_and_the_insider_path_is_extra() {
    let (queue, mut jobs) = InsiderQueue::channel(4);
    let (emitter, mut events) = super::live_emit::Emitter::channel(4);
    let handling = Handling {
        insiders: Some(&queue),
        emitter: Some(&emitter),
        ..Handling::default()
    };
    let handled = handle_message_full(
        &message(&[raw(CEO_EN)]),
        Utc::now(),
        &metso_on("XHEL"),
        &mut SeenSet::new(10),
        &handling,
    )
    .await;
    let super::live::Disposition::New(item) = &handled.dispositions[0] else {
        panic!("new item expected");
    };
    assert_eq!(item.insider, Some("queued"));
    assert_eq!(item.emit, Some("queued"));
    assert!(
        item.insider_release.is_none(),
        "the release moved to the queue"
    );
    assert_eq!(
        events.try_recv().unwrap().event.event_type,
        "company_announcement"
    );
    assert_eq!(jobs.try_recv().unwrap().item.news_id.to_string(), CEO_EN);
}

#[tokio::test]
async fn english_then_finnish_and_the_reverse_give_identical_messages() {
    let companies = metso_on("XHEL");
    let en_first = live(
        &companies,
        &[message(&[raw(CEO_EN)]), message(&[raw(CEO_FI)])],
    )
    .await;
    let fi_first = live(
        &companies,
        &[message(&[raw(CEO_FI)]), message(&[raw(CEO_EN)])],
    )
    .await;
    assert_eq!(en_first.len(), 2);
    assert_eq!(fi_first.len(), 2);
    let ids = |s: &Sent| keyed(s).iter().map(identity).collect::<Vec<_>>();
    let all: Vec<Identity> = ids(&en_first).into_iter().chain(ids(&fi_first)).collect();
    assert!(all.iter().all(|i| *i == all[0]), "{all:#?}");
    assert_eq!(
        keyed(&en_first).iter().map(lang).collect::<Vec<_>>(),
        ["en", "fi"]
    );
    assert_eq!(
        keyed(&fi_first).iter().map(lang).collect::<Vec<_>>(),
        ["fi", "en"]
    );
    assert_eq!(
        identity(&poll(&[news(&raw(CEO_EN)), news(&raw(CEO_FI))])[0]),
        all[0]
    );
}

#[tokio::test]
async fn a_buy_by_a_close_associate_is_identical_in_both_languages() {
    let en = variant("6a75a7ee-2271-5a16-9aae-4dc5c1221331", "en", WARTSILA_EN);
    let fi = variant("6a75a7ee-2271-5a16-9aae-4dc5c1221332", "fi", WARTSILA_FI);
    let companies = metso_on("XHEL");
    let en_first = live(
        &companies,
        &[
            message(std::slice::from_ref(&en)),
            message(std::slice::from_ref(&fi)),
        ],
    )
    .await;
    let fi_first = live(&companies, &[message(&[fi]), message(&[en])]).await;
    let (a, b): (Vec<_>, Vec<_>) = keyed(&en_first).into_iter().partition(|m| lang(m) == "en");
    assert_eq!(a.len(), 2, "two trades per copy");
    assert_eq!(
        a.iter().map(identity).collect::<Vec<_>>(),
        b.iter().map(identity).collect::<Vec<_>>()
    );
    let (c, d): (Vec<_>, Vec<_>) = keyed(&fi_first).into_iter().partition(|m| lang(m) == "en");
    assert_eq!(
        c.iter().map(identity).collect::<Vec<_>>(),
        a.iter().map(identity).collect::<Vec<_>>()
    );
    assert_eq!(
        d.iter().map(identity).collect::<Vec<_>>(),
        a.iter().map(identity).collect::<Vec<_>>()
    );
    assert!(a.iter().all(|(_, m)| m.price_per_unit > 30.0));
    assert_eq!(
        a[0].1.transaction_type,
        model::generated::UnifiedTransactionType::Buy as i32
    );
}

#[tokio::test]
async fn swedish_companies_get_no_insider_message() {
    for mic in ["XSTO", "FNSE"] {
        let (queue, mut jobs) = InsiderQueue::channel(4);
        let handling = Handling {
            insiders: Some(&queue),
            ..Handling::default()
        };
        let mut item = raw(CEO_EN);
        item["author"]["tickers"] = serde_json::json!([format!("{mic}:METSO")]);
        let handled = handle_message_full(
            &message(&[item]),
            Utc::now(),
            &metso_on(mic),
            &mut SeenSet::new(10),
            &handling,
        )
        .await;
        assert_eq!(handled.dispositions.len(), 1, "{mic}: still relevant");
        let super::live::Disposition::New(item) = &handled.dispositions[0] else {
            panic!("new item expected");
        };
        assert_eq!(item.insider, None, "{mic}");
        assert!(jobs.try_recv().is_err(), "{mic}");
    }
}

async fn metered<F: std::future::Future>(f: F) -> String {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(
                "nordic_live_insider_latency_seconds".into(),
            ),
            INSIDER_LATENCY_BUCKETS,
        )
        .unwrap()
        .build_recorder();
    let handle = recorder.handle();
    let _guard = metrics::set_default_local_recorder(&recorder);
    f.await;
    handle.render()
}

fn value(text: &str, series: &str) -> f64 {
    text.lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("series {series} not found in:\n{text}"))
        .parse()
        .unwrap()
}

#[tokio::test]
async fn an_unparsable_release_is_counted_and_the_stream_goes_on() {
    let garbage = variant(
        "11111111-2222-5333-8444-555555555555",
        "en",
        "Person subject to the notification requirement\nnothing else",
    );
    let pre_mar = {
        let mut i = variant("11111111-2222-5333-8444-555555555556", "en", "");
        i["content"] = raw(CEO_EN)["content"].clone();
        i["content"]["publish_date"] = serde_json::json!("2016-07-02T10:00:00Z");
        i
    };
    let zero_buy = {
        let mut i = raw(CEO_EN);
        i["news_id"] = serde_json::json!("11111111-2222-5333-8444-555555555557");
        let text = i["content"]["text"]
            .as_str()
            .unwrap()
            .replace("Receipt of a Share-Based Incentive", "Acquisition");
        i["content"]["text"] = serde_json::json!(text);
        i
    };
    let mut sent = Vec::new();
    let text = metered(async {
        sent = live(
            &metso_on("XHEL"),
            &[
                message(&[garbage]),
                message(&[pre_mar]),
                message(&[zero_buy]),
                message(&[raw(CEO_EN)]),
            ],
        )
        .await;
    })
    .await;
    assert_eq!(sent.len(), 1, "the good release after the bad ones");
    assert_eq!(sent[0].1.person_name, "Sami Takaluoma");
    let series = |o: &str| {
        value(
            &text,
            &format!("nordic_live_insider_transactions_total{{outcome=\"{o}\",country=\"FI\"}}"),
        )
    };
    assert_eq!(series("parse_error"), 1.0);
    assert_eq!(series("skipped"), 2.0, "pre-MAR and the zero-price buy");
    assert_eq!(series("published"), 1.0);
    assert_eq!(
        value(&text, "nordic_live_insider_latency_seconds_count"),
        1.0
    );
}

#[tokio::test]
async fn a_full_queue_drops_and_counts_without_blocking_the_reader() {
    let (queue, _jobs) = InsiderQueue::channel(1);
    let handling = Handling {
        insiders: Some(&queue),
        ..Handling::default()
    };
    let companies = metso_on("XHEL");
    let mut seen = SeenSet::new(10);
    let mut outcomes = Vec::new();
    let text = metered(async {
        for id in [CEO_EN, CEO_FI] {
            let handled = tokio::time::timeout(
                Duration::from_secs(1),
                handle_message_full(
                    &message(&[raw(id)]),
                    Utc::now(),
                    &companies,
                    &mut seen,
                    &handling,
                ),
            )
            .await
            .expect("the reader never waits on the queue");
            let super::live::Disposition::New(item) = &handled.dispositions[0] else {
                panic!("new item expected");
            };
            outcomes.push(item.insider);
        }
    })
    .await;
    assert_eq!(outcomes, [Some("queued"), Some("dropped")]);
    assert_eq!(
        value(
            &text,
            "nordic_live_insider_transactions_total{outcome=\"dropped\",country=\"FI\"}"
        ),
        1.0
    );
}

#[tokio::test]
async fn without_a_queue_the_insider_path_is_disabled() {
    let handled = handle_message_full(
        &message(&[raw(CEO_EN)]),
        Utc::now(),
        &metso_on("XHEL"),
        &mut SeenSet::new(10),
        &Handling::default(),
    )
    .await;
    let super::live::Disposition::New(item) = &handled.dispositions[0] else {
        panic!("new item expected");
    };
    assert_eq!(item.insider, Some("disabled"));
}

#[tokio::test]
async fn catch_up_items_take_the_same_path() {
    let (queue, jobs) = InsiderQueue::channel(64);
    let handling = Handling {
        insiders: Some(&queue),
        ..Handling::default()
    };
    let items: Vec<NewsItem> = parse_document(&message(&raw_items())).unwrap().items;
    let mut ordered = items.clone();
    ordered.sort_by_key(|i| i.published_at);
    handle_catchup_items(
        ordered,
        Utc::now(),
        &metso_on("XHEL"),
        &mut SeenSet::new(100),
        &handling,
    )
    .await;
    drop(queue);
    let sink = Arc::new(VecSink::default());
    run_insider_producer(sink.clone(), jobs, fast()).await;
    let sent = keyed(&sink.sent.lock().unwrap());
    assert_eq!(sent.len(), 20, "both copies of ten releases");
    let mut live_ids: Vec<Identity> = sent.iter().map(identity).collect();
    live_ids.sort_by(|a, b| a.key.cmp(&b.key));
    live_ids.dedup();
    let mut poll_ids: Vec<Identity> = poll(&items).iter().map(identity).collect();
    poll_ids.sort_by(|a, b| a.key.cmp(&b.key));
    assert_eq!(
        live_ids, poll_ids,
        "live, deduplicated by signal's key, is the poll's set"
    );
}

#[test]
fn parsing_one_release_is_build_on_that_item() {
    let item = news(&raw(CEO_FI));
    let parsed = parse_release(&metso(), &item);
    assert_eq!(parsed.outcome(), "published");
    assert_eq!(parsed.messages, poll(&[item]));
}

/// The CEO release in `lang` with another id and the volume `23201` replaced.
fn with_volume(base: &str, news_id: &str, volume: &str) -> serde_json::Value {
    let mut item = raw(base);
    item["news_id"] = serde_json::json!(news_id);
    let text = item["content"]["text"]
        .as_str()
        .unwrap()
        .replace("23201", volume);
    item["content"]["text"] = serde_json::json!(text);
    item
}

fn volumes(s: &Sent) -> Vec<(String, f64)> {
    keyed(s).iter().map(|m| (lang(m), m.1.volume)).collect()
}

#[tokio::test]
async fn a_twin_that_parses_differently_is_not_published_in_either_order() {
    let companies = metso_on("XHEL");
    let fi_off = with_volume(CEO_FI, "0d603af2-5858-58ee-a4e4-821966dc8201", "23200");
    let mut sent = Vec::new();
    let text = metered(async {
        sent = live(
            &companies,
            &[
                message(&[raw(CEO_EN)]),
                message(std::slice::from_ref(&fi_off)),
            ],
        )
        .await;
    })
    .await;
    assert_eq!(volumes(&sent), [("en".to_string(), 23201.0)]);
    assert_eq!(
        value(
            &text,
            "nordic_live_insider_transactions_total{outcome=\"conflict\",country=\"FI\"}"
        ),
        1.0
    );
    let sent = live(&companies, &[message(&[fi_off]), message(&[raw(CEO_EN)])]).await;
    assert_eq!(
        volumes(&sent),
        [("fi".to_string(), 23200.0)],
        "the first copy wins, as the first (reference, index) does in the poll"
    );
}

#[tokio::test]
async fn a_correction_reusing_the_reference_number_is_not_published() {
    let mut correction = with_volume(CEO_EN, "9954de80-88dd-5084-a03b-4b42e17c1645", "23000");
    correction["content"]["title"] =
        serde_json::json!("Metso Corporation: Correction: Managers' transactions - Sami Takaluoma");
    correction["content"]["publish_date"] = serde_json::json!("2026-03-21T08:00:00Z");
    let sent = live(
        &metso_on("XHEL"),
        &[message(&[raw(CEO_EN)]), message(&[correction])],
    )
    .await;
    assert_eq!(volumes(&sent), [("en".to_string(), 23201.0)]);
    let identical = with_volume(CEO_EN, "9954de80-88dd-5084-a03b-4b42e17c1646", "23201");
    let sent = live(
        &metso_on("XHEL"),
        &[message(&[raw(CEO_EN)]), message(&[identical])],
    )
    .await;
    assert_eq!(sent.len(), 2, "an identical repeat is harmless and is sent");
}

#[test]
fn the_reference_memory_forgets_the_oldest_beyond_its_capacity() {
    use super::live_insiders::{Admission, SentReferences};
    let en = parse_release(&metso(), &news(&raw(CEO_EN))).messages;
    let other = parse_release(
        &metso(),
        &news(&raw("0915c0ba-5a9d-5c7e-a193-d997c45833b9")),
    )
    .messages;
    let mut changed = en.clone();
    changed[0].1.volume = 1.0;
    let mut sent = SentReferences::new(1);
    assert_eq!(sent.admit(&en), Admission::First);
    assert_eq!(sent.admit(&en), Admission::Repeat);
    assert_eq!(sent.admit(&changed), Admission::Conflict);
    assert_eq!(sent.admit(&other), Admission::First);
    assert_eq!(sent.admit(&changed), Admission::First, "forgotten");
}
