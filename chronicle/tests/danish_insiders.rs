//! Danish managers' transactions (NEX-282): ESMA-template releases on MFN
//! parsed into `DK_MAR` insider messages, by the poll and by nordic-live.
//!
//! `fixtures/mfn/danish/managers_transactions.json` holds 16 real MFN items
//! (Novo Nordisk, Demant, Rockwool, GN Store Nord, Royal Unibrew, Torm, Vestas):
//! English and Danish forms, language twins, several persons and transaction
//! blocks in one release, an amendment, wrapped table cells, a plain layout, a
//! PDF-only release, an old short form, and a buyback and a major-shareholder
//! notice that must not be selected.

use async_trait::async_trait;
use backon::ExponentialBuilder;
use chronicle_lib::nordic::db::Company;
use chronicle_lib::nordic::esma_release::{self, parse_number};
use chronicle_lib::nordic::feed::{parse_document, NewsItem};
use chronicle_lib::nordic::insiders::{self, insider_key, trades_identity, InsiderSink};
use chronicle_lib::nordic::live::{
    handle_message_full, Companies, Disposition, Handling, Registered, SeenSet,
};
use chronicle_lib::nordic::live_insiders::{parse_release, run_insider_producer, InsiderQueue};
use chronicle_lib::nordic::mar_release::MarParseError;
use chrono::Utc;
use model::generated::{
    unified_insider_transaction::SourceDetail, SourceRegistry, UnifiedInsiderTransaction,
    UnifiedTransactionType,
};
use model::nordic::Lang;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FIXTURE: &str = include_str!("fixtures/mfn/danish/managers_transactions.json");
const DANISH: &[Lang] = &[Lang::Da, Lang::En];

const NOVO_SALE: &str = "99c0b2db";
const NOVO_AMENDMENT: &str = "52d3f633";
const DEMANT_DA: &str = "8635234f";
const DEMANT_EN: &str = "1bf179f5";
const DEMANT_THREE_DA: &str = "65fd9ba2";
const DEMANT_THREE_EN: &str = "785afbad";
const GN_TWO_BLOCKS: &str = "ebb20cc2";
const ROCKWOOL_PLAIN_DA: &str = "683ee1ae";
const ROCKWOOL_PLAIN_EN: &str = "8363f250";
const ROCKWOOL_BAD_DA: &str = "551a0412";
const ROCKWOOL_BAD_EN: &str = "41169366";
const ROYAL_UNIBREW_WRAPPED: &str = "72aea637";
const VESTAS_PDF: &str = "d1c589a2";
const VESTAS_BUYBACK: &str = "2baf40c9";
const VESTAS_MAJOR: &str = "1d9a6bf7";
const TORM_SHORT_FORM: &str = "5d861fe0";

fn raw_items() -> Vec<serde_json::Value> {
    serde_json::from_str(FIXTURE).unwrap()
}

fn raw(prefix: &str) -> serde_json::Value {
    raw_items()
        .into_iter()
        .find(|i| i["news_id"].as_str().unwrap().starts_with(prefix))
        .unwrap_or_else(|| panic!("no fixture {prefix}"))
}

fn message(items: &[serde_json::Value]) -> String {
    serde_json::json!({ "items": items }).to_string()
}

fn news(prefix: &str) -> NewsItem {
    parse_document(&message(&[raw(prefix)]))
        .unwrap()
        .items
        .remove(0)
}

fn company(ticker: &str, slug: &str) -> Company {
    Company {
        ticker: ticker.into(),
        exchange_mic: "XCSE".into(),
        mfn_slug: Some(slug.into()),
        name: None,
        isin: None,
    }
}

fn novo() -> Company {
    company("NOVO-B", "novo-nordisk")
}

fn demant() -> Company {
    company("DEMANT", "demant")
}

fn rockwool() -> Company {
    company("ROCK-B", "rockwool")
}

fn detail(m: &UnifiedInsiderTransaction) -> &model::generated::MarReleaseDetail {
    match m.source_detail.as_ref() {
        Some(SourceDetail::Mar(d)) => d,
        other => panic!("expected MarReleaseDetail, got {other:?}"),
    }
}

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn parse(
    prefix: &str,
) -> Result<Vec<chronicle_lib::nordic::mar_release::ParsedMarTransaction>, MarParseError> {
    esma_release::parse(news(prefix).text.as_deref().unwrap(), DANISH)
}

#[test]
fn an_english_form_becomes_one_dk_mar_message() {
    let release = parse_release(&novo(), &news(NOVO_SALE));
    assert_eq!(release.outcome(), "published");
    assert_eq!(release.messages.len(), 1);
    let (key, m) = &release.messages[0];
    assert_eq!(key, "DK_MAR:NOVO-B:MARTIN HOLST LANGE:2026-11-04");
    assert_eq!(key, &insider_key(m));
    assert_eq!(m.source_registry, SourceRegistry::DkMar as i32);
    assert_eq!(
        (m.ticker.as_str(), m.exchange_mic.as_str()),
        ("NOVO-B", "XCSE")
    );
    assert_eq!(m.person_name, "Martin Holst Lange");
    assert_eq!(
        m.person_role,
        "Executive Vice President, R&D and Chief Scientific Officer (CSO)"
    );
    assert_eq!(m.transaction_type, UnifiedTransactionType::Sell as i32);
    assert_eq!(
        (m.volume, m.price_per_unit, m.currency.as_str()),
        (12000.0, 285.0, "DKK")
    );
    assert_eq!(m.transaction_date, "2026-11-04");
    assert_eq!(m.published_date, "2026-11-05", "the Copenhagen date");
    assert!(!m.is_amendment);
    let d = detail(m);
    assert_eq!(
        (d.volume_decimal.as_str(), d.unit_price_decimal.as_str()),
        ("12000", "285.00")
    );
    assert_eq!(d.isin, "DK0062498333");
    assert_eq!(d.instrument_type, "Shares");
    assert_eq!(d.nature, "Sale of shares");
    assert_eq!(d.venue, "Nasdaq Copenhagen");
    assert_eq!(d.lei, "549300DAQ1CVT6CXN342");
    assert_eq!(d.lang, "en");
    assert!(d.reference_number.is_empty(), "the template has none");
}

#[test]
fn a_danish_form_reads_danish_numbers_and_agrees_with_its_english_twin() {
    let da = parse(DEMANT_DA).unwrap();
    let en = parse(DEMANT_EN).unwrap();
    assert_eq!(da.len(), 1);
    let t = &da[0];
    assert_eq!(t.person_name, "Niels Wagner");
    assert_eq!(t.person_role, "President of Hearing Care");
    assert_eq!(t.instrument_type, "Aktier");
    assert_eq!(t.isin, "DK0060738599");
    assert_eq!(t.nature, "Salg");
    assert_eq!(t.transaction_type, UnifiedTransactionType::Sell);
    assert_eq!(t.unit_price, Some(dec("294.87")), "DKK 294,87");
    assert_eq!(t.volume, dec("5000"), "5.000 stk. aktier");
    assert_eq!(
        t.transaction_date.to_string(),
        "2026-08-18",
        "18. august 2026"
    );
    assert_eq!(t.venue.as_deref(), Some("Nasdaq Copenhagen A/S"));
    assert_eq!(t.lang, "da");
    assert_eq!(
        (en[0].unit_price, en[0].volume, en[0].transaction_date),
        (t.unit_price, t.volume, t.transaction_date),
        "the English copy writes 294.87 and 5,000"
    );
    assert_eq!(en[0].lang, "en");
}

#[test]
fn the_poll_keeps_the_english_copy_of_a_twin() {
    let items = [news(DEMANT_DA), news(DEMANT_EN)];
    let refs: Vec<&NewsItem> = items.iter().collect();
    let batch = insiders::build(&demant(), &refs);
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(batch.stats.twins_skipped, 1);
    assert_eq!(detail(&batch.messages[0].1).lang, "en");
}

#[test]
fn several_transaction_blocks_and_pairs_in_one_release() {
    let ts = parse(GN_TWO_BLOCKS).unwrap();
    assert_eq!(ts.len(), 8);
    let first: Vec<_> = ts
        .iter()
        .filter(|t| t.transaction_date.to_string() == "2025-05-15")
        .collect();
    assert_eq!(first.len(), 7);
    assert_eq!(
        first.iter().map(|t| t.volume).sum::<Decimal>(),
        dec("6904"),
        "the stated aggregated volume"
    );
    assert!(first.iter().all(|t| t.unit_price == Some(dec("93.66"))));
    let second = &ts[7];
    assert_eq!(second.transaction_date.to_string(), "2025-05-16");
    assert_eq!(
        (second.unit_price, second.volume),
        (Some(dec("98.34")), dec("96"))
    );
    assert!(ts
        .iter()
        .all(|t| t.person_name == "Jørgen Bundgaard Hansen"));
    assert_eq!(
        ts.iter().map(|t| t.index).collect::<Vec<_>>(),
        (0..8).collect::<Vec<_>>()
    );
}

#[test]
fn several_persons_in_one_release_in_both_languages() {
    for id in [DEMANT_THREE_EN, DEMANT_THREE_DA] {
        let ts = parse(id).unwrap();
        let people: Vec<(&str, Decimal)> = ts
            .iter()
            .map(|t| (t.person_name.as_str(), t.volume))
            .collect();
        assert_eq!(
            people,
            [
                ("Søren Nielsen", dec("36749")),
                ("René Schneider", dec("17750")),
                ("Niels Wagner", dec("14161"))
            ],
            "{id}"
        );
        assert!(ts
            .iter()
            .all(|t| t.transaction_type == UnifiedTransactionType::Grant));
    }
}

#[test]
fn an_amendment_carries_the_originals_transaction_id() {
    let release = parse_release(&novo(), &news(NOVO_AMENDMENT));
    let (key, m) = &release.messages[0];
    assert!(m.is_amendment);
    assert_eq!(m.amended_transaction_id, *key);
    assert_eq!(key, "DK_MAR:NOVO-B:CHOI LAI CHRISTINA LAW:2024-11-06");
    assert_eq!((m.volume, m.price_per_unit), (1400.0, 753.02));
    assert!(detail(m)
        .amendment_reason
        .contains("erroneously stated a price"));
}

#[test]
fn a_plain_layout_with_a_small_price_table() {
    let da = parse(ROCKWOOL_PLAIN_DA).unwrap();
    let en = parse(ROCKWOOL_PLAIN_EN).unwrap();
    assert_eq!(da[0].person_name, "Kim Junge Andersen");
    assert_eq!(da[0].instrument_type, "B-aktie");
    assert_eq!(da[0].isin, "DK0063855168");
    assert_eq!(
        (da[0].unit_price, da[0].volume),
        (Some(Decimal::ZERO), dec("6511"))
    );
    assert_eq!(da[0].transaction_type, UnifiedTransactionType::Grant);
    assert_eq!(
        (en[0].unit_price, en[0].volume, en[0].transaction_type),
        (da[0].unit_price, da[0].volume, da[0].transaction_type)
    );
}

#[test]
fn prices_wrapped_inside_narrow_cells() {
    let ts = parse(ROYAL_UNIBREW_WRAPPED).unwrap();
    let pairs: Vec<_> = ts.iter().map(|t| (t.unit_price, t.volume)).collect();
    assert_eq!(
        pairs,
        [
            (Some(dec("415")), dec("1888")),
            (Some(dec("403.40")), dec("1000"))
        ]
    );
}

#[test]
fn a_misplaced_separator_refuses_the_copy_and_the_twin_is_used() {
    assert!(matches!(
        parse(ROCKWOOL_BAD_DA),
        Err(MarParseError::InvalidNumber { .. })
    ));
    let items = [news(ROCKWOOL_BAD_DA), news(ROCKWOOL_BAD_EN)];
    let refs: Vec<&NewsItem> = items.iter().collect();
    let batch = insiders::build(&rockwool(), &refs);
    assert_eq!(batch.stats.parse_errors, 1);
    assert_eq!(batch.messages.len(), 7);
    assert!(batch
        .messages
        .iter()
        .any(|(_, m)| m.price_per_unit == 200.932));
    let associates = batch
        .messages
        .iter()
        .filter(|(_, m)| detail(m).is_close_associate)
        .count();
    assert_eq!(associates, 6);
}

#[test]
fn a_pdf_only_release_is_counted_not_parsed() {
    let release = parse_release(&company("VWS", "vestas-wind-systems"), &news(VESTAS_PDF));
    assert!(release.messages.is_empty());
    assert_eq!(release.outcome(), "form_not_in_text");
    assert_eq!(release.stats.form_not_in_text, 1);
    assert_eq!(parse(VESTAS_PDF), Err(MarParseError::FormNotInText));
}

#[test]
fn the_old_short_form_is_counted_apart() {
    let release = parse_release(&company("TRMD-A", "torm"), &news(TORM_SHORT_FORM));
    assert_eq!(release.outcome(), "unsupported_form");
    assert_eq!(parse(TORM_SHORT_FORM), Err(MarParseError::UnsupportedForm));
}

fn titled(title: &str) -> NewsItem {
    NewsItem {
        title: title.into(),
        text: None,
        ..news(NOVO_SALE)
    }
}

#[test]
fn selection_by_real_headlines_in_both_directions() {
    let yes = [
        "Novo Nordisk A/S: Trading in Novo Nordisk shares by board members, executives and associated persons",
        "Bestyrelsesmedlemmers, koncerndirektørers og nærtstående personers handel med Novo Nordisk-aktier den 1. februar 2018",
        "Demant A/S: Ledende medarbejderes og disses nærtståendes transaktioner med Demant-aktier",
        "Demant A/S: Transactions with Demant shares by managers and closely associated parties",
        "Indberetning af ledende medarbejders og disses nærtståendes transaktioner med ROCKWOOL A/S aktier",
        "Vestas Wind Systems A/S: Notification of Executive's transactions with Vestas shares",
        "Vestas Wind Systems A/S: Rapportering af ledende medarbejders transaktion med Vestas-aktier",
        "Torm PLC: Report of transactions in TORM plc securities by directors and executive officers and their closely associated persons",
        "Transactions with shares and linked securities in Genmab A/S made by managerial employees and their closely associated persons",
        "REPORTING OF TRANSACTIONS MADE BY PERSONS DISCHARGING MANAGERIAL RESPONSIBILITIES",
        "Notification of Executive\u{2019}s transaction with Matas shares",
        "Transactions of executives and related parties in NKT shares",
        "Danske Bank A/S, ledende medarbejderes transaktioner",
        "Report on insiders trade with FirstFarms A/S' shares",
        "Indberetning af insideres handel med FirstFarms A/S' aktier",
    ];
    for t in yes {
        assert!(insiders::selects("XCSE", &titled(t)), "{t}");
    }
    let no = [
        "Vestas - Transactions in connection with share buy-back programme, week 46 2026",
        "Novo Nordisk A/S - share repurchase programme",
        "Torm PLC: Major Shareholder Announcement",
        "Storaktionærmeddelelse - Henrik Hougaard",
        "Reduction of share capital in Vestas Wind Systems A/S by cancellation of treasury shares",
        "Gennemførelse af kapitalnedsættelse i Vestas Wind Systems A/S ved annullering af egne aktier",
        "Launch of Long-Term Incentive Plan for Executive Management and Key Employees",
        "Novo raises full-year outlook on continued GLP-1 momentum",
    ];
    for t in no {
        assert!(!insiders::selects("XCSE", &titled(t)), "{t}");
    }
    assert!(!insiders::selects("XCSE", &news(VESTAS_BUYBACK)));
    assert!(!insiders::selects("XCSE", &news(VESTAS_MAJOR)));
    let mut form_only = news(NOVO_SALE);
    form_only.title = "Novo Nordisk A/S: Company announcement".into();
    assert!(insiders::selects("FNDK", &form_only), "the form text alone");
}

#[test]
fn danish_and_english_number_formats() {
    let cases = [
        ("1.234,56", true, "1234.56"),
        ("1,234.56", true, "1234.56"),
        ("1.234,56", false, "1234.56"),
        ("1,234.56", false, "1234.56"),
        ("5.000", true, "5000"),
        ("5,000", false, "5000"),
        ("294,87", true, "294.87"),
        ("294.87", false, "294.87"),
        ("200,932", true, "200.932"),
        ("200.932", false, "200.932"),
        ("1.474.326,60", true, "1474326.60"),
        ("3,420,000.00", false, "3420000.00"),
        ("12000", false, "12000"),
    ];
    for (raw, decimal_comma, want) in cases {
        assert_eq!(
            parse_number(raw, decimal_comma),
            Some(dec(want)),
            "{raw} {decimal_comma}"
        );
    }
    assert_eq!(parse_number("DKK", false), None);
}

#[test]
fn a_finnish_release_is_unaffected() {
    let insiders: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "fixtures/mfn/finnish/news_metso_insiders.json"
    ))
    .unwrap();
    let ceo = insiders
        .into_iter()
        .find(|i| i["news_id"] == "9954de80-88dd-5084-a03b-4b42e17c1644")
        .unwrap();
    let item = parse_document(&message(&[ceo])).unwrap().items.remove(0);
    let metso = Company {
        ticker: "METSO".into(),
        exchange_mic: "XHEL".into(),
        mfn_slug: Some("metso-2".into()),
        name: None,
        isin: None,
    };
    assert!(insiders::selects("XHEL", &item));
    let release = parse_release(&metso, &item);
    let (key, m) = &release.messages[0];
    assert_eq!(key, "FIN_MAR:METSO:SAMI TAKALUOMA:2026-03-19");
    assert_eq!(m.source_registry, SourceRegistry::FinMar as i32);
    assert!(!detail(m).reference_number.is_empty());
}

// ---------------------------------------------------------------------------
// Live

type Sent = Vec<(String, UnifiedInsiderTransaction)>;

#[derive(Default)]
struct VecSink {
    sent: Mutex<Sent>,
}

#[async_trait]
impl InsiderSink for VecSink {
    async fn send(&self, key: &str, txn: &UnifiedInsiderTransaction) -> anyhow::Result<()> {
        self.sent
            .lock()
            .unwrap()
            .push((key.to_string(), txn.clone()));
        Ok(())
    }
}

fn registered(ticker: &str, mic: &str, slug: &str) -> Companies {
    Companies::new(vec![Registered {
        ticker: ticker.into(),
        exchange_mic: mic.into(),
        mfn_slug: slug.into(),
        name: None,
    }])
}

async fn live(companies: &Companies, ids: &[&str]) -> Sent {
    let (queue, jobs) = InsiderQueue::channel(16);
    let handling = Handling {
        insiders: Some(&queue),
        ..Handling::default()
    };
    let mut seen = SeenSet::new(100);
    for id in ids {
        handle_message_full(
            &message(&[raw(id)]),
            Utc::now(),
            companies,
            &mut seen,
            &handling,
        )
        .await;
    }
    drop(queue);
    let sink = Arc::new(VecSink::default());
    let backoff = ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(1))
        .with_total_delay(Some(Duration::from_millis(10)));
    run_insider_producer(sink.clone(), jobs, backoff).await;
    let sent = std::mem::take(&mut *sink.sent.lock().unwrap());
    sent
}

#[tokio::test]
async fn a_danish_release_read_live_is_what_the_poll_publishes() {
    let sent = live(&registered("NOVO-B", "XCSE", "novo-nordisk"), &[NOVO_SALE]).await;
    let poll = parse_release(&novo(), &news(NOVO_SALE)).messages;
    assert_eq!(sent, poll);
    assert_eq!(sent[0].0, "DK_MAR:NOVO-B:MARTIN HOLST LANGE:2026-11-04");
}

#[tokio::test]
async fn twins_in_either_order_give_the_same_trades() {
    let companies = registered("DEMANT", "XCSE", "demant");
    let da_first = live(&companies, &[DEMANT_DA, DEMANT_EN]).await;
    let en_first = live(&companies, &[DEMANT_EN, DEMANT_DA]).await;
    assert_eq!(da_first.len(), 2, "the identical second copy is sent too");
    assert_eq!(en_first.len(), 2);
    let ids = |s: &Sent| {
        let mut v = trades_identity(s);
        v.sort();
        v
    };
    assert_eq!(ids(&da_first), ids(&en_first));
    let mut all = ids(&da_first);
    all.dedup();
    assert_eq!(all.len(), 1, "one trade, one signal row");
}

#[tokio::test]
async fn a_refused_first_copy_leaves_the_twin_to_publish() {
    let sent = live(
        &registered("ROCK-B", "XCSE", "rockwool"),
        &[ROCKWOOL_BAD_DA, ROCKWOOL_BAD_EN],
    )
    .await;
    assert_eq!(sent.len(), 7);
    assert!(sent.iter().all(|(_, m)| detail(m).lang == "en"));
}

#[tokio::test]
async fn a_danish_item_is_still_an_announcement_and_queued_for_insiders() {
    let (queue, mut jobs) = InsiderQueue::channel(4);
    let handling = Handling {
        insiders: Some(&queue),
        ..Handling::default()
    };
    let handled = handle_message_full(
        &message(&[raw(NOVO_SALE)]),
        Utc::now(),
        &registered("NOVO-B", "XCSE", "novo-nordisk"),
        &mut SeenSet::new(10),
        &handling,
    )
    .await;
    let Disposition::New(item) = &handled.dispositions[0] else {
        panic!("new item expected");
    };
    assert_eq!(item.insider, Some("queued"));
    assert!(item.announcement.is_some(), "still a company_announcement");
    assert_eq!(jobs.try_recv().unwrap().company.exchange_mic, "XCSE");
}

#[tokio::test]
async fn a_pdf_only_release_live_publishes_nothing() {
    let sent = live(
        &registered("VWS", "XCSE", "vestas-wind-systems"),
        &[VESTAS_PDF],
    )
    .await;
    assert!(sent.is_empty());
}

#[test]
fn a_sale_to_cover_tax_is_a_sale_and_a_gift_a_gift() {
    let form = |nature: &str| {
        news(NOVO_SALE)
            .text
            .unwrap()
            .replace("Sale of shares", nature)
    };
    let kind =
        |nature: &str| esma_release::parse(&form(nature), DANISH).unwrap()[0].transaction_type;
    assert_eq!(
        kind(
            "Sale of shares (to cover tax on shares vested under the Restricted Shares programme)"
        ),
        UnifiedTransactionType::Sell
    );
    assert_eq!(
        kind("Shares received as gift"),
        UnifiedTransactionType::Gift
    );
    assert_eq!(kind("Purchase of shares"), UnifiedTransactionType::Buy);
    assert_eq!(
        kind(
            "Other transaction (vesting of shares in accordance with Restricted Shares programme)"
        ),
        UnifiedTransactionType::Grant
    );
}

fn with(prefix: &str, news_id: &str, from: &str, to: &str) -> serde_json::Value {
    let mut item = raw(prefix);
    item["news_id"] = serde_json::json!(news_id);
    let text = item["content"]["text"].as_str().unwrap().replace(from, to);
    item["content"]["text"] = serde_json::json!(text);
    item
}

async fn live_raw(companies: &Companies, items: &[serde_json::Value]) -> Sent {
    let (queue, jobs) = InsiderQueue::channel(16);
    let handling = Handling {
        insiders: Some(&queue),
        ..Handling::default()
    };
    let mut seen = SeenSet::new(100);
    for item in items {
        handle_message_full(
            &message(std::slice::from_ref(item)),
            Utc::now(),
            companies,
            &mut seen,
            &handling,
        )
        .await;
    }
    drop(queue);
    let sink = Arc::new(VecSink::default());
    let backoff = ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(1))
        .with_total_delay(Some(Duration::from_millis(10)));
    run_insider_producer(sink.clone(), jobs, backoff).await;
    let sent = std::mem::take(&mut *sink.sent.lock().unwrap());
    sent
}

#[tokio::test]
async fn same_language_releases_with_identical_trades_by_two_persons_are_both_sent() {
    let other = with(
        NOVO_SALE,
        "99c0b2db-0000-4000-8000-000000000001",
        "Martin Holst Lange",
        "Another Executive",
    );
    let sent = live_raw(
        &registered("NOVO-B", "XCSE", "novo-nordisk"),
        &[raw(NOVO_SALE), other],
    )
    .await;
    let keys: Vec<&str> = sent.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        [
            "DK_MAR:NOVO-B:MARTIN HOLST LANGE:2026-11-04",
            "DK_MAR:NOVO-B:ANOTHER EXECUTIVE:2026-11-04"
        ],
        "not a twin: same language, so the second is a release of its own"
    );
}

fn replaced(m: &UnifiedInsiderTransaction) -> &str {
    detail(m).replaced_person_name.as_str()
}

#[tokio::test]
async fn a_twin_spelt_differently_names_the_replaced_spelling_live_and_in_the_poll() {
    let da = with(
        DEMANT_DA,
        "8635234f-0000-4000-8000-000000000001",
        "Niels Wagner",
        "Niels Wagnér",
    );
    let companies = registered("DEMANT", "XCSE", "demant");
    let sent = live_raw(&companies, &[da.clone(), raw(DEMANT_EN)]).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0, "DK_MAR:DEMANT:NIELS WAGNÉR:2026-08-18");
    assert_eq!(replaced(&sent[0].1), "");
    assert_eq!(sent[1].0, "DK_MAR:DEMANT:NIELS WAGNER:2026-08-18");
    assert_eq!(
        replaced(&sent[1].1),
        "Niels Wagnér",
        "signal replaces that row"
    );

    let english_first = live_raw(&companies, &[raw(DEMANT_EN), da.clone()]).await;
    assert_eq!(
        english_first.len(),
        1,
        "the later Danish spelling is not sent"
    );
    assert_eq!(replaced(&english_first[0].1), "");

    let items: Vec<NewsItem> = parse_document(&message(&[da, raw(DEMANT_EN)]))
        .unwrap()
        .items;
    let refs: Vec<&NewsItem> = items.iter().collect();
    let batch = insiders::build(&demant(), &refs);
    assert_eq!(batch.stats.twins_skipped, 1);
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(batch.messages[0].1.person_name, "Niels Wagner");
    assert_eq!(replaced(&batch.messages[0].1), "Niels Wagnér");
}

/// Two releases of one day, X's and Y's, with identical trades, each in
/// Danish and English (review of PR #336).
fn two_releases() -> [serde_json::Value; 4] {
    let x_da = raw(DEMANT_DA);
    let x_en = raw(DEMANT_EN);
    let y_da = with(
        DEMANT_DA,
        "8635234f-0000-4000-8000-0000000000b1",
        "Niels Wagner",
        "Søren Nielsen",
    );
    let y_en = with(
        DEMANT_EN,
        "1bf179f5-0000-4000-8000-0000000000b2",
        "Niels Wagner",
        "Søren Nielsen",
    );
    [x_da, y_da, x_en, y_en]
}

#[tokio::test]
async fn two_persons_with_identical_trades_in_both_languages_are_never_twins() {
    let companies = registered("DEMANT", "XCSE", "demant");
    let [x_da, y_da, x_en, y_en] = two_releases();
    let orders = [
        vec![x_da.clone(), y_da.clone(), x_en.clone(), y_en.clone()],
        vec![x_da.clone(), y_da.clone(), y_en.clone(), x_en.clone()],
        vec![x_da.clone(), x_en.clone(), y_da.clone(), y_en.clone()],
        vec![y_en.clone(), x_da.clone(), x_en.clone(), y_da.clone()],
        vec![x_da.clone(), y_en.clone(), y_da.clone(), x_en.clone()],
    ];
    for order in orders {
        let sent = live_raw(&companies, &order).await;
        let mut people: Vec<&str> = sent.iter().map(|(_, m)| m.person_name.as_str()).collect();
        people.sort();
        people.dedup();
        assert_eq!(people, ["Niels Wagner", "Søren Nielsen"]);
        assert!(
            sent.iter().all(|(_, m)| replaced(m).is_empty()),
            "nobody replaces anybody"
        );
        assert_eq!(sent.len(), 4, "every copy agrees with its own twin");
    }
    let items: Vec<NewsItem> = parse_document(&message(&two_releases())).unwrap().items;
    let refs: Vec<&NewsItem> = items.iter().collect();
    let batch = insiders::build(&demant(), &refs);
    let mut people: Vec<&str> = batch
        .messages
        .iter()
        .map(|(_, m)| m.person_name.as_str())
        .collect();
    people.sort();
    assert_eq!(people, ["Niels Wagner", "Søren Nielsen"]);
    assert_eq!(batch.stats.twins_skipped, 2);
    assert!(batch.messages.iter().all(|(_, m)| replaced(m).is_empty()));
}

#[test]
fn which_spellings_can_name_one_person() {
    use chronicle_lib::nordic::insiders::similar_names;
    assert!(similar_names("Niels Wagnér", "Niels Wagner"));
    assert!(similar_names("SØREN NIELSEN", "Søren Nielsen"));
    assert!(similar_names(
        "Sisse Fjelsted Rasmussen",
        "Sisse Fjelsted Jacobsen"
    ));
    assert!(!similar_names("Anders Hansen", "Anders Jensen"));
    assert!(!similar_names("Niels Wagner", "Søren Nielsen"));
    assert!(!similar_names(
        "Sisse Fjelsted Rasmussen",
        "Karen Fjelsted Rasmussen"
    ));
    assert!(!similar_names("Ann Marie Lund Holm", "Ann Mette Lund Hald"));
}
