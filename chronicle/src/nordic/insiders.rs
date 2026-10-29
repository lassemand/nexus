//! Finnish managers' transactions from MFN releases (NEX-207).
//!
//! Finland has no insider register, so each issuer's MAR Art. 19 release is the
//! source. `nordic-reports` already reads every item of a company's feed; the
//! items that are managers' transactions are routed here, parsed by
//! [`super::mar_release`] and turned into `UnifiedInsiderTransaction`s for
//! `insider.transactions`. Swedish companies are not handled here (FI's register
//! covers them, see `pdmr.rs`) and Danish insiders are out of scope.
//!
//! Every release comes twice (Finnish and English) with the same MAR reference
//! number. The English copy is preferred; the Finnish one is used only when no
//! English copy with the same reference was read.

use super::classify::strip_issuer_prefix;
use super::db::Company;
use super::feed::NewsItem;
use super::mar_release::{parse, MarParseError, ParsedMarTransaction, ReleaseContext};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use model::generated::UnifiedInsiderTransaction;
use model::insider::transaction_id;
use model::nordic::{self, InsiderSourceKind};
use prost::Message;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use std::collections::HashSet;
use std::time::Duration;
use tracing::{debug, warn};

/// Topic signal consumes insider transactions from.
pub const INSIDER_TRANSACTIONS_TOPIC: &str = "insider.transactions";

/// Source label in the Kafka key, as `pdmr.rs` uses `FI` and `SEC`.
pub const KEY_SOURCE: &str = "FIN_MAR";

/// MAR applies from 2016-07-03; earlier releases use the old Finnish format.
pub fn mar_start() -> NaiveDate {
    NaiveDate::from_ymd_opt(2016, 7, 3).expect("valid date")
}

/// Phrases of a managers' transaction headline, lower case.
const TITLE_PHRASES: &[&str] = &[
    "managers' transaction",
    "managers\u{2019} transaction",
    "johtohenkilöiden liiketoimet",
    "johdon liiketoimet",
];

/// Phrases that open the notification form in the release text, lower case.
const TEXT_PHRASES: &[&str] = &[
    "person subject to the notification requirement",
    "ilmoitusvelvollinen",
];

/// True for listings whose country takes its insider transactions from issuers'
/// MAR releases on MFN ([`InsiderSourceKind::MarReleases`]).
pub fn uses_mar_releases(exchange_mic: &str) -> bool {
    nordic::country_for_mic(exchange_mic)
        .is_some_and(|c| c.insider_source == InsiderSourceKind::MarReleases)
}

/// True if the item is a managers' transaction release: the headline (after the
/// issuer prefix) says so, or the text holds the notification form.
pub fn is_managers_transaction(item: &NewsItem) -> bool {
    let title = strip_issuer_prefix(&item.title).to_lowercase();
    let full = item.title.to_lowercase();
    // The double prefix ("Metso Corporation: Metso Oyj: Johtohenkilöiden ...")
    // leaves one prefix after stripping, so the full title is checked too.
    if TITLE_PHRASES
        .iter()
        .any(|p| title.contains(p) || full.contains(p))
    {
        return true;
    }
    item.text.as_deref().is_some_and(|t| {
        let t = t.to_lowercase();
        TEXT_PHRASES.iter().any(|p| t.contains(p))
    })
}

/// Counts of one [`build`], for logs and metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InsiderStats {
    /// Managers' transaction items seen.
    pub items: usize,
    /// Dated before MAR (2016-07-03) and ignored.
    pub before_mar: usize,
    /// In a language other than `fi` / `en`, ignored.
    pub other_language: usize,
    /// Could not be parsed (and have no parsed twin).
    pub parse_errors: usize,
    /// Rejected for a zero price on a BUY or SELL.
    pub suspicious_zero_price: usize,
    /// Finnish copies skipped because the English copy was read.
    pub twins_skipped: usize,
    /// Transactions dropped as repeats of `(reference_number, index)`.
    pub duplicates: usize,
}

/// The messages to publish for a set of items, oldest first.
#[derive(Debug, Default)]
pub struct Batch {
    /// `(publish_date of the release, message)`.
    pub messages: Vec<(DateTime<Utc>, UnifiedInsiderTransaction)>,
    /// What was seen and dropped.
    pub stats: InsiderStats,
}

/// The name after the last " - " of a headline, lower case.
fn trailing_name(title: &str) -> Option<String> {
    title
        .rsplit_once(" - ")
        .map(|(_, n)| n.trim().to_lowercase())
        .filter(|n| !n.is_empty())
}

struct Parsed<'a> {
    item: &'a NewsItem,
    transactions: Vec<ParsedMarTransaction>,
}

/// Why an item produced no transactions.
enum Failure {
    NoText,
    Empty,
    Parse(MarParseError),
}

impl Failure {
    fn is_zero_price(&self) -> bool {
        matches!(
            self,
            Failure::Parse(MarParseError::SuspiciousZeroPrice { .. })
        )
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NoText => write!(f, "the feed item has no text"),
            Failure::Empty => write!(f, "no transactions"),
            Failure::Parse(e) => write!(f, "{e}"),
        }
    }
}

fn parse_item(item: &NewsItem, lang: &str) -> Result<Vec<ParsedMarTransaction>, Failure> {
    let text = item.text.as_deref().ok_or(Failure::NoText)?;
    match parse(text, lang) {
        Ok(t) if t.is_empty() => Err(Failure::Empty),
        Ok(t) => Ok(t),
        Err(e) => Err(Failure::Parse(e)),
    }
}

/// Parses `items` (all managers' transactions of one Finnish company) into the
/// messages to publish. Pure: nothing is sent.
///
/// Per MAR reference number the English copy wins, the Finnish one is a
/// fallback, and `(reference_number, index)` is unique in the result.
pub fn build(company: &Company, items: &[&NewsItem]) -> Batch {
    let mut stats = InsiderStats::default();
    let mut en: Vec<Parsed> = Vec::new();
    let mut fi: Vec<Parsed> = Vec::new();
    let mut failed: Vec<(&NewsItem, Failure)> = Vec::new();

    for &item in items {
        stats.items += 1;
        if item.published_at.date_naive() < mar_start() {
            stats.before_mar += 1;
            continue;
        }
        let lang = match item.lang.as_deref() {
            Some(l @ ("en" | "fi")) => l,
            _ => {
                stats.other_language += 1;
                continue;
            }
        };
        match parse_item(item, lang) {
            Ok(transactions) => {
                let p = Parsed { item, transactions };
                if lang == "en" {
                    en.push(p)
                } else {
                    fi.push(p)
                }
            }
            Err(e) => failed.push((item, e)),
        }
    }

    let english_refs: HashSet<&str> = en
        .iter()
        .map(|p| p.transactions[0].reference_number.as_str())
        .collect();
    let mut chosen: Vec<&Parsed> = en.iter().collect();
    for p in &fi {
        if english_refs.contains(p.transactions[0].reference_number.as_str()) {
            stats.twins_skipped += 1;
        } else {
            chosen.push(p);
        }
    }

    // A copy that does not parse is not an error when its twin in the other
    // language (same instant, same person in the headline) did.
    for (item, err) in &failed {
        let other = if item.lang.as_deref() == Some("fi") {
            &en
        } else {
            &fi
        };
        let covered = trailing_name(&item.title).is_some_and(|name| {
            other.iter().any(|p| {
                p.item.published_at == item.published_at
                    && trailing_name(&p.item.title).as_deref() == Some(name.as_str())
            })
        });
        if covered {
            debug!(news_id = %item.news_id, "a copy did not parse, its twin in the other language did");
            continue;
        }
        let zero = err.is_zero_price();
        if zero {
            stats.suspicious_zero_price += 1;
        } else {
            stats.parse_errors += 1;
        }
        warn!(
            ticker = %company.ticker,
            news_id = %item.news_id,
            url = item.url.as_deref().unwrap_or(""),
            error = %err,
            suspicious_zero_price = zero,
            "managers' transaction not parsed, skipped"
        );
    }

    chosen.sort_by_key(|p| (p.item.published_at, p.item.news_id));
    let mut seen: HashSet<(String, usize)> = HashSet::new();
    let mut messages = Vec::new();
    for p in chosen {
        let published_date =
            model::exchange::local_date(&company.exchange_mic, p.item.published_at)
                .unwrap_or_else(|| p.item.published_at.date_naive())
                .format("%Y-%m-%d")
                .to_string();
        let news_id = p.item.news_id.to_string();
        for t in &p.transactions {
            if !seen.insert((t.reference_number.clone(), t.index)) {
                stats.duplicates += 1;
                continue;
            }
            let msg = t.clone().into_unified(ReleaseContext {
                ticker: &company.ticker,
                exchange_mic: &company.exchange_mic,
                published_date: &published_date,
                mfn_news_id: &news_id,
                url: p.item.url.as_deref().unwrap_or(""),
            });
            messages.push((p.item.published_at, msg));
        }
    }
    Batch { messages, stats }
}

/// The Kafka key of a message, the convention of `pdmr.rs`:
/// `{SOURCE}:{TICKER}:{PERSON}:{DATE}`.
pub fn insider_key(txn: &UnifiedInsiderTransaction) -> String {
    transaction_id(
        KEY_SOURCE,
        &txn.ticker,
        &txn.person_name,
        &txn.transaction_date,
    )
}

/// Where insider messages go: Kafka in production, a vector in tests.
#[async_trait]
pub trait InsiderSink: Send + Sync {
    /// Delivers one message; returns once the broker acknowledged it.
    async fn send(&self, key: &str, txn: &UnifiedInsiderTransaction) -> anyhow::Result<()>;
}

/// Publishes to a Kafka topic.
pub struct KafkaInsiderSink {
    producer: FutureProducer,
    topic: String,
}

impl KafkaInsiderSink {
    /// Connects a producer for `topic` on `brokers`.
    pub fn new(brokers: &str, topic: impl Into<String>) -> anyhow::Result<Self> {
        let producer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "10000")
            .create()?;
        Ok(Self {
            producer,
            topic: topic.into(),
        })
    }
}

#[async_trait]
impl InsiderSink for KafkaInsiderSink {
    async fn send(&self, key: &str, txn: &UnifiedInsiderTransaction) -> anyhow::Result<()> {
        let payload = txn.encode_to_vec();
        self.producer
            .send(
                FutureRecord::to(&self.topic).key(key).payload(&payload),
                Duration::from_secs(10),
            )
            .await
            .map_err(|(e, _)| anyhow::anyhow!("publishing insider transaction {key}: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nordic::feed::parse_document;
    use model::generated::{unified_insider_transaction::SourceDetail, SourceRegistry};

    /// Real Metso releases: the 18 of 2026-04-24T13:00Z (9 people x fi/en) and
    /// Sami Takaluoma's of 2026-03-20 (fi/en), with their text.
    const INSIDERS: &str =
        include_str!("../../tests/fixtures/mfn/finnish/news_metso_insiders.json");
    /// Metso's reports, without text.
    const REPORTS: &str = include_str!("../../tests/fixtures/mfn/finnish/news_metso.json");

    fn items(fixture: &str) -> Vec<NewsItem> {
        parse_document(&format!(r#"{{"items": {fixture}}}"#))
            .unwrap()
            .items
    }

    fn metso() -> Company {
        Company {
            ticker: "METSO".into(),
            exchange_mic: "XHEL".into(),
            mfn_slug: Some("metso-2".into()),
            name: Some("Metso".into()),
            isin: Some("FI0009014575".into()),
        }
    }

    fn refs(items: &[NewsItem]) -> Vec<&NewsItem> {
        items.iter().collect()
    }

    fn detail(m: &UnifiedInsiderTransaction) -> &model::generated::MarReleaseDetail {
        match m.source_detail.as_ref() {
            Some(SourceDetail::Mar(d)) => d,
            other => panic!("expected MarReleaseDetail, got {other:?}"),
        }
    }

    #[test]
    fn headline_or_form_text_selects_managers_transactions() {
        let all = items(INSIDERS);
        assert_eq!(all.len(), 20);
        assert!(all.iter().all(is_managers_transaction));
        // Headline alone, both languages and the double prefix.
        let mut no_text = all.clone();
        for i in &mut no_text {
            i.text = None;
        }
        assert!(no_text.iter().all(is_managers_transaction));
        let titles: Vec<_> = no_text.iter().map(|i| i.title.as_str()).collect();
        assert!(titles
            .iter()
            .any(|t| t.starts_with("Metso Corporation: Managers' transactions - ")));
        assert!(titles
            .iter()
            .any(|t| t.contains("Metso Oyj: Johtohenkilöiden liiketoimet - ")));
        // The form text alone.
        let mut form_only = all[0].clone();
        form_only.title = "Metso Corporation: Release".into();
        assert!(is_managers_transaction(&form_only));
        form_only.text = Some("Ilmoitusvelvollinen\\nNimi: X".replace("\\n", "\n"));
        assert!(is_managers_transaction(&form_only));
        form_only.text = Some("Interim report".into());
        assert!(!is_managers_transaction(&form_only));
        // Reports are not.
        assert!(!items(REPORTS)
            .iter()
            .any(|i| i.title.contains("Interim Report") && is_managers_transaction(i)));
        let curly = NewsItem {
            title: "Orion Corporation: Managers\u{2019} transactions \u{2013} Sophie Papa".into(),
            text: None,
            ..all[0].clone()
        };
        assert!(is_managers_transaction(&curly));
    }

    #[test]
    fn only_finnish_companies_get_insiders() {
        assert!(uses_mar_releases("XHEL") && uses_mar_releases("FNFI"));
        for m in ["XSTO", "FNSE", "XCSE", "FNDK", "XNAS"] {
            assert!(!uses_mar_releases(m), "{m}");
        }
    }

    #[test]
    fn the_april_burst_is_nine_people_once_each() {
        let all = items(INSIDERS);
        let burst: Vec<&NewsItem> = all
            .iter()
            .filter(|i| i.published_at.to_rfc3339() == "2026-04-24T13:00:00+00:00")
            .collect();
        assert_eq!(burst.len(), 18);
        let batch = build(&metso(), &burst);
        assert_eq!(batch.messages.len(), 9, "{:?}", batch.stats);
        let people: HashSet<&str> = batch
            .messages
            .iter()
            .map(|(_, m)| m.person_name.as_str())
            .collect();
        assert_eq!(people.len(), 9);
        assert_eq!(batch.stats.twins_skipped, 9);
        assert_eq!(batch.stats.parse_errors, 0);
        for (_, m) in &batch.messages {
            assert_eq!(m.source_registry, SourceRegistry::FinMar as i32);
            assert_eq!(
                (
                    m.ticker.as_str(),
                    m.exchange_mic.as_str(),
                    m.currency.as_str()
                ),
                ("METSO", "XHEL", "EUR")
            );
            assert_eq!(m.transaction_date, "2026-04-23");
            assert_eq!(m.published_date, "2026-04-24");
            assert_eq!(
                m.transaction_type,
                model::generated::UnifiedTransactionType::Grant as i32
            );
            assert_eq!(m.price_per_unit, 0.0, "an incentive is not a buy price");
            assert!(m.volume > 0.0);
            let d = detail(m);
            assert_eq!(d.lang, "en", "the English copy is read");
            assert_eq!(d.unit_price_decimal, "0.00");
            assert!(d.reference_number.starts_with("549300R0VN9C371W0E07_"));
            assert!(d.url.starts_with("https://mfn.se/"));
            assert_eq!(d.mfn_news_id.len(), 36);
            assert_eq!(
                insider_key(m),
                format!("FIN_MAR:METSO:{}:2026-04-23", m.person_name.to_uppercase())
            );
        }
    }

    #[test]
    fn the_ceo_release_and_the_finnish_fallback() {
        let all = items(INSIDERS);
        let ceo: Vec<&NewsItem> = all
            .iter()
            .filter(|i| i.title.contains("Takaluoma"))
            .collect();
        assert_eq!(ceo.len(), 2);
        let b = build(&metso(), &ceo);
        assert_eq!(b.messages.len(), 1);
        let m = &b.messages[0].1;
        assert_eq!(
            (m.person_name.as_str(), m.person_role.as_str()),
            ("Sami Takaluoma", "Chief Executive Officer")
        );
        assert_eq!(
            (m.transaction_date.as_str(), m.published_date.as_str()),
            ("2026-03-19", "2026-03-20")
        );
        assert_eq!((m.volume, m.price_per_unit), (23201.0, 0.0));
        assert_eq!(detail(m).volume_decimal, "23201");
        assert_eq!(detail(m).nature, "Receipt of a Share-Based Incentive");
        // Without the English copy the Finnish one is used.
        let fi_only: Vec<&NewsItem> = ceo
            .iter()
            .copied()
            .filter(|i| i.lang.as_deref() == Some("fi"))
            .collect();
        let b = build(&metso(), &fi_only);
        assert_eq!(b.messages.len(), 1);
        assert_eq!(detail(&b.messages[0].1).lang, "fi");
        assert_eq!(b.messages[0].1.person_role, "Toimitusjohtaja");
        assert_eq!(b.messages[0].1.volume, 23201.0);
        // An English copy that does not parse is covered by its Finnish twin.
        let mut broken = all.clone();
        for i in broken
            .iter_mut()
            .filter(|i| i.title.contains("Takaluoma") && i.lang.as_deref() == Some("en"))
        {
            i.text = Some("garbage".into());
        }
        let ceo: Vec<&NewsItem> = broken
            .iter()
            .filter(|i| i.title.contains("Takaluoma"))
            .collect();
        let b = build(&metso(), &ceo);
        assert_eq!(
            (b.messages.len(), b.stats.parse_errors),
            (1, 0),
            "{:?}",
            b.stats
        );
    }

    #[test]
    fn published_date_is_the_helsinki_date() {
        let all = items(INSIDERS);
        let mut ceo: Vec<NewsItem> = all
            .iter()
            .filter(|i| i.title.contains("Takaluoma"))
            .cloned()
            .collect();
        // 23:30Z on the 20th is already the 21st in Helsinki (UTC+2 in March).
        for i in &mut ceo {
            i.published_at = "2026-03-20T23:30:00Z".parse().unwrap();
        }
        let b = build(&metso(), &refs(&ceo));
        assert_eq!(b.messages[0].1.published_date, "2026-03-21");
        // Summer: 21:30Z is 00:30 the next day (UTC+3).
        for i in &mut ceo {
            i.published_at = "2026-07-20T21:30:00Z".parse().unwrap();
        }
        let b = build(&metso(), &refs(&ceo));
        assert_eq!(b.messages[0].1.published_date, "2026-07-21");
    }

    #[test]
    fn releases_before_mar_are_counted_and_ignored() {
        let all = items(INSIDERS);
        let mut ceo: Vec<NewsItem> = all
            .iter()
            .filter(|i| i.title.contains("Takaluoma"))
            .cloned()
            .collect();
        for i in &mut ceo {
            i.published_at = "2016-07-02T10:00:00Z".parse().unwrap();
        }
        let b = build(&metso(), &refs(&ceo));
        assert!(b.messages.is_empty());
        assert_eq!(b.stats.before_mar, 2);
        for i in &mut ceo {
            i.published_at = "2016-07-03T10:00:00Z".parse().unwrap();
        }
        assert_eq!(build(&metso(), &refs(&ceo)).messages.len(), 1);
    }

    #[test]
    fn repeats_are_dropped_by_reference_and_index() {
        let all = items(INSIDERS);
        let en: Vec<NewsItem> = all
            .iter()
            .filter(|i| i.title.contains("Takaluoma") && i.lang.as_deref() == Some("en"))
            .cloned()
            .collect();
        // The same release twice, e.g. read on two pages around a boundary.
        let twice = vec![en[0].clone(), en[0].clone()];
        let b = build(&metso(), &refs(&twice));
        assert_eq!((b.messages.len(), b.stats.duplicates), (1, 1));
    }

    #[test]
    fn unparsable_and_zero_price_releases_are_counted_apart_and_do_not_stop_the_rest() {
        let all = items(INSIDERS);
        let mut set: Vec<NewsItem> = all
            .iter()
            .filter(|i| i.lang.as_deref() == Some("en"))
            .cloned()
            .collect();
        assert_eq!(set.len(), 10);
        set[0].text = Some("not a notification".into());
        // A zero price on an acquisition is never published as a buy.
        set[1].text = set[1]
            .text
            .as_ref()
            .map(|t| t.replace("Receipt of a Share-Based Incentive", "Acquisition"));
        let b = build(&metso(), &refs(&set));
        assert_eq!(b.stats.parse_errors, 1, "{:?}", b.stats);
        assert_eq!(b.stats.suspicious_zero_price, 1, "{:?}", b.stats);
        assert_eq!(b.messages.len(), 8);
        assert!(b.messages.iter().all(
            |(_, m)| m.transaction_type != model::generated::UnifiedTransactionType::Buy as i32
        ));
        // No text at all counts as a parse error too.
        set[2].text = None;
        assert_eq!(build(&metso(), &refs(&set)).stats.parse_errors, 2);
    }

    #[test]
    fn messages_come_oldest_first() {
        let all = items(INSIDERS);
        let b = build(&metso(), &refs(&all));
        assert_eq!(b.messages.len(), 10);
        let times: Vec<_> = b.messages.iter().map(|(t, _)| *t).collect();
        let mut sorted = times.clone();
        sorted.sort();
        assert_eq!(times, sorted);
        assert_eq!(b.messages[0].1.person_name, "Sami Takaluoma");
    }
}
