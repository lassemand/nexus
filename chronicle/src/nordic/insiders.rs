//! Managers' transactions from MFN releases: Finland (NEX-207) and Denmark
//! (NEX-282).
//!
//! Neither country has a register we read, so each issuer's MAR Art. 19 release
//! is the source. `nordic-reports` already reads every item of a company's feed;
//! the items that are managers' transactions are routed here, parsed and turned
//! into `UnifiedInsiderTransaction`s for `insider.transactions`. Swedish
//! companies are not handled here (FI's register covers them, see `pdmr.rs`).
//!
//! The country's [`InsiderSourceKind`] picks the [`InsiderDialect`]:
//!
//! * **Finnish** ([`InsiderSourceKind::MarReleases`]): the Finnish FSA's layout,
//!   parsed by [`super::mar_release`]. Every release comes twice (Finnish and
//!   English) with the same MAR reference number. The English copy is
//!   preferred; the Finnish one is used only when no English copy with the same
//!   reference was read.
//! * **ESMA** ([`InsiderSourceKind::EsmaReleases`], Denmark): ESMA's template,
//!   parsed by [`super::esma_release`]. The template has no reference number, so
//!   a local-language copy is skipped when an English copy of the same release
//!   gives identical trades; a copy without a twin is used as it is.

use super::classify::strip_issuer_prefix;
use super::db::Company;
use super::esma_release;
use super::events::trace_headers;
use super::feed::NewsItem;
use super::mar_release::{parse, MarParseError, ParsedMarTransaction, ReleaseContext};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use model::generated::{SourceRegistry, UnifiedInsiderTransaction};
use model::insider::transaction_id;
use model::nordic::{self, InsiderSourceKind, Lang};
use prost::Message;
use rdkafka::message::Headers;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tracing::{debug, warn, Span};

/// Topic signal consumes insider transactions from.
pub const INSIDER_TRANSACTIONS_TOPIC: &str = "insider.transactions";

/// Source label in the Kafka key, as `pdmr.rs` uses `FI` and `SEC`.
pub const KEY_SOURCE: &str = "FIN_MAR";

/// Source label of the Danish (ESMA template) messages.
pub const DK_KEY_SOURCE: &str = "DK_MAR";

/// How a country's managers' transaction releases are written, from its
/// [`InsiderSourceKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsiderDialect {
    /// The Finnish FSA's layout, with a reference number.
    Finnish,
    /// ESMA's template (Commission Implementing Regulation (EU) 2016/523).
    Esma,
}

impl InsiderDialect {
    /// The dialect of the country listing `exchange_mic`, if it takes its
    /// insider transactions from issuers' releases on MFN.
    pub fn for_mic(exchange_mic: &str) -> Option<Self> {
        match nordic::country_for_mic(exchange_mic)?.insider_source {
            InsiderSourceKind::MarReleases => Some(Self::Finnish),
            InsiderSourceKind::EsmaReleases => Some(Self::Esma),
            InsiderSourceKind::FiRegister | InsiderSourceKind::None => None,
        }
    }

    /// The registry of its messages.
    pub fn registry(self) -> SourceRegistry {
        match self {
            Self::Finnish => SourceRegistry::FinMar,
            Self::Esma => SourceRegistry::DkMar,
        }
    }
}

/// True for listings whose country takes its insider transactions from
/// issuers' releases on MFN, in either dialect.
pub fn takes_releases(exchange_mic: &str) -> bool {
    InsiderDialect::for_mic(exchange_mic).is_some()
}

/// True if `item`, from a company listed on `exchange_mic`, is a managers'
/// transaction release in its country's dialect and languages.
pub fn selects(exchange_mic: &str, item: &NewsItem) -> bool {
    match InsiderDialect::for_mic(exchange_mic) {
        Some(InsiderDialect::Finnish) => is_managers_transaction(item),
        Some(InsiderDialect::Esma) => {
            esma_release::is_managers_transaction(item, country_languages(exchange_mic))
        }
        None => false,
    }
}

fn country_languages(exchange_mic: &str) -> &'static [Lang] {
    nordic::country_for_mic(exchange_mic).map_or(&[], |c| c.languages)
}

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
    /// The notification form is not in the text (a PDF attachment).
    pub form_not_in_text: usize,
    /// In the older short form without a unit price.
    pub unsupported_form: usize,
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
    if InsiderDialect::for_mic(&company.exchange_mic) == Some(InsiderDialect::Esma) {
        return build_esma(company, items);
    }
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

/// Messages with their Kafka keys.
type Keyed = Vec<(String, UnifiedInsiderTransaction)>;

/// The messages of the ESMA dialect, oldest first. Per release, an English
/// copy wins over a local-language copy with identical trades.
fn build_esma(company: &Company, items: &[&NewsItem]) -> Batch {
    let mut stats = InsiderStats::default();
    let langs = country_languages(&company.exchange_mic);
    let english = Lang::En.code();
    let mut parsed: Vec<(&NewsItem, String, Keyed)> = Vec::new();
    let mut seen_ids: HashSet<uuid::Uuid> = HashSet::new();
    for &item in items {
        stats.items += 1;
        if !seen_ids.insert(item.news_id) {
            stats.duplicates += 1;
            continue;
        }
        if item.published_at.date_naive() < mar_start() {
            stats.before_mar += 1;
            continue;
        }
        let known = item
            .lang
            .as_deref()
            .and_then(Lang::from_code)
            .is_some_and(|l| langs.contains(&l));
        if !known {
            stats.other_language += 1;
            continue;
        }
        let Some(text) = item.text.as_deref() else {
            stats.form_not_in_text += 1;
            continue;
        };
        match esma_release::parse(text, langs) {
            Ok(transactions) => {
                let messages = esma_messages(company, item, transactions);
                let lang = item.lang.clone().unwrap_or_default();
                parsed.push((item, lang, messages));
            }
            Err(e) => {
                match e {
                    MarParseError::FormNotInText => stats.form_not_in_text += 1,
                    MarParseError::UnsupportedForm => stats.unsupported_form += 1,
                    MarParseError::SuspiciousZeroPrice { .. } => stats.suspicious_zero_price += 1,
                    _ => stats.parse_errors += 1,
                }
                let quiet = matches!(
                    e,
                    MarParseError::FormNotInText | MarParseError::UnsupportedForm
                );
                if quiet {
                    debug!(ticker = %company.ticker, news_id = %item.news_id, error = %e,
                           "managers' transaction without a parsable form in the text, skipped");
                } else {
                    warn!(ticker = %company.ticker, news_id = %item.news_id,
                          url = item.url.as_deref().unwrap_or(""), error = %e,
                          "managers' transaction not parsed, skipped");
                }
            }
        }
    }
    // English copies first, so a local copy is compared with every English copy
    // of its release.
    let (mut english_copies, local): (Vec<_>, Vec<_>) =
        parsed.into_iter().partition(|(_, lang, _)| lang == english);
    let mut seen: HashMap<String, Vec<(SeenCopy, usize)>> = HashMap::new();
    for (k, (item, lang, m)) in english_copies.iter().enumerate() {
        if let Some(f) = esma_fingerprint(&company.exchange_mic, item, m) {
            seen.entry(f).or_default().push((SeenCopy::of(lang, m), k));
        }
    }
    let mut chosen_local = Vec::new();
    for (item, lang, m) in local {
        let Some(f) = esma_fingerprint(&company.exchange_mic, item, &m) else {
            chosen_local.push((item, lang, m));
            continue;
        };
        let copies = seen.entry(f).or_default();
        let copy = SeenCopy::of(&lang, &m);
        let known: Vec<SeenCopy> = copies.iter().map(|(c, _)| c.clone()).collect();
        match match_twin(&known, &copy) {
            TwinMatch::Repeat(i) | TwinMatch::Conflict(i) => {
                stats.twins_skipped += 1;
                copies[i].0.paired = true;
            }
            TwinMatch::Misspelt(i) => {
                stats.twins_skipped += 1;
                copies[i].0.paired = true;
                let english_index = copies[i].1;
                mark_replaced(&mut english_copies[english_index].2, &copy);
            }
            TwinMatch::New => chosen_local.push((item, lang, m)),
        }
    }
    let mut chosen: Vec<_> = english_copies.into_iter().chain(chosen_local).collect();
    chosen.sort_by_key(|(item, _, _)| (item.published_at, item.news_id));
    let messages = chosen
        .into_iter()
        .flat_map(|(item, _, m)| m.into_iter().map(move |(_, msg)| (item.published_at, msg)))
        .collect();
    Batch { messages, stats }
}

fn esma_messages(
    company: &Company,
    item: &NewsItem,
    transactions: Vec<ParsedMarTransaction>,
) -> Vec<(String, UnifiedInsiderTransaction)> {
    let published_date = model::exchange::local_date(&company.exchange_mic, item.published_at)
        .unwrap_or_else(|| item.published_at.date_naive())
        .format("%Y-%m-%d")
        .to_string();
    let news_id = item.news_id.to_string();
    transactions
        .into_iter()
        .map(|t| {
            let amended = t.is_amendment.then(|| {
                transaction_id(
                    DK_KEY_SOURCE,
                    &company.ticker,
                    &t.person_name,
                    &t.transaction_date.format("%Y-%m-%d").to_string(),
                )
            });
            let msg = t.into_unified_as(
                ReleaseContext {
                    ticker: &company.ticker,
                    exchange_mic: &company.exchange_mic,
                    published_date: &published_date,
                    mfn_news_id: &news_id,
                    url: item.url.as_deref().unwrap_or(""),
                },
                SourceRegistry::DkMar,
                amended.unwrap_or_default(),
            );
            (insider_key(&msg), msg)
        })
        .collect()
}

/// What identifies one release across its language copies: the MAR reference
/// number (Finnish dialect), or for the ESMA template, which has none, the
/// release's local publish date with each trade's date, volume, price and
/// currency (names and nature texts are left out: they are translated, and an
/// issuer's typo in one copy must not split the pair). `None` when there is
/// nothing to identify.
pub fn release_reference(
    exchange_mic: &str,
    item: &NewsItem,
    messages: &[(String, UnifiedInsiderTransaction)],
) -> Option<String> {
    match InsiderDialect::for_mic(exchange_mic)? {
        InsiderDialect::Finnish => messages.first().and_then(|(_, m)| match &m.source_detail {
            Some(model::generated::unified_insider_transaction::SourceDetail::Mar(d))
                if !d.reference_number.is_empty() =>
            {
                Some(d.reference_number.clone())
            }
            _ => None,
        }),
        InsiderDialect::Esma => esma_fingerprint(exchange_mic, item, messages),
    }
}

fn esma_fingerprint(
    exchange_mic: &str,
    item: &NewsItem,
    messages: &[(String, UnifiedInsiderTransaction)],
) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    let day = model::exchange::local_date(exchange_mic, item.published_at)
        .unwrap_or_else(|| item.published_at.date_naive());
    let mut trades: Vec<String> = messages
        .iter()
        .map(|(_, m)| {
            format!(
                "{}|{}|{}|{}",
                m.transaction_date, m.volume, m.price_per_unit, m.currency
            )
        })
        .collect();
    trades.sort();
    Some(format!(
        "{DK_KEY_SOURCE}:{}:{day}#{}",
        item_ticker(messages),
        trades.join(";")
    ))
}

fn item_ticker(messages: &[(String, UnifiedInsiderTransaction)]) -> &str {
    messages.first().map_or("", |(_, m)| m.ticker.as_str())
}

/// One copy of a release already sent (live) or chosen (poll), for
/// [`match_twin`].
#[derive(Debug, Clone, PartialEq)]
pub struct SeenCopy {
    /// The copy's language.
    pub lang: String,
    /// Each trade's person, in order.
    pub names: Vec<String>,
    /// [`trades_identity`] of the copy.
    pub identity: Vec<String>,
    /// Already matched with a copy in the other language.
    pub paired: bool,
}

impl SeenCopy {
    /// The copy of `messages`, in `lang`.
    pub fn of(lang: &str, messages: &[(String, UnifiedInsiderTransaction)]) -> Self {
        Self {
            lang: lang.to_string(),
            names: messages
                .iter()
                .map(|(_, m)| m.person_name.clone())
                .collect(),
            identity: trades_identity(messages),
            paired: false,
        }
    }
}

/// How a copy relates to the copies seen of releases with its fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwinMatch {
    /// The same trades as copy `i`: a repeat or a twin that agrees.
    Repeat(usize),
    /// Copy `i` in the other language has the same persons but other trades
    /// (a nature translated into another type): a twin that disagrees.
    Conflict(usize),
    /// Copy `i` in the other language is this copy's twin with persons spelt
    /// differently: no seen copy has this copy's names, every name is a
    /// variant of copy `i`'s ([`similar_names`]), and copy `i` is the only
    /// unpaired copy in the other language that fits.
    Misspelt(usize),
    /// Another release (other persons with identical trades).
    New,
}

/// Matches `copy` against `seen`, the copies of releases with the same ESMA
/// fingerprint. Twins are recognised by their persons, never by equal trades
/// alone: two persons with identical trades on one day are two releases.
pub fn match_twin(seen: &[SeenCopy], copy: &SeenCopy) -> TwinMatch {
    if let Some(i) = seen.iter().position(|c| c.identity == copy.identity) {
        return TwinMatch::Repeat(i);
    }
    if let Some(i) = seen.iter().position(|c| c.names == copy.names) {
        return if seen[i].lang != copy.lang && !seen[i].paired {
            TwinMatch::Conflict(i)
        } else {
            TwinMatch::New
        };
    }
    let candidates: Vec<usize> = seen
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.lang != copy.lang
                && !c.paired
                && c.names.len() == copy.names.len()
                && c.names
                    .iter()
                    .zip(&copy.names)
                    .all(|(a, b)| similar_names(a, b))
        })
        .map(|(i, _)| i)
        .collect();
    match candidates.as_slice() {
        [i] => TwinMatch::Misspelt(*i),
        _ => TwinMatch::New,
    }
}

/// True if two spellings can name one person: equal after folding case and
/// diacritics (`Wagnér` / `Wagner`), or three or more words with the same first
/// word and exactly one other word different (`Sisse Fjelsted Rasmussen` /
/// `Sisse Fjelsted Jacobsen`). Two-word names differing in a word are two
/// persons (`Anders Hansen` / `Anders Jensen`).
pub fn similar_names(a: &str, b: &str) -> bool {
    let fold = |s: &str| -> Vec<String> {
        s.split_whitespace()
            .map(|w| {
                w.to_lowercase()
                    .chars()
                    .map(|c| match c {
                        'á' | 'à' | 'â' | 'ä' | 'å' | 'ã' => "a".to_string(),
                        'é' | 'è' | 'ê' | 'ë' => "e".to_string(),
                        'í' | 'ì' | 'î' | 'ï' => "i".to_string(),
                        'ó' | 'ò' | 'ô' | 'ö' | 'ø' | 'õ' => "o".to_string(),
                        'ú' | 'ù' | 'û' | 'ü' => "u".to_string(),
                        'æ' => "ae".to_string(),
                        'ñ' => "n".to_string(),
                        'ç' => "c".to_string(),
                        c => c.to_string(),
                    })
                    .collect()
            })
            .collect()
    };
    let (x, y) = (fold(a), fold(b));
    if x == y {
        return true;
    }
    x.len() >= 3
        && x.len() == y.len()
        && x[0] == y[0]
        && x.iter().zip(&y).filter(|(p, q)| p != q).count() == 1
}

/// Sets `replaced_person_name` on each of `messages` whose person `earlier`
/// (the twin copy, in order) wrote differently.
pub fn mark_replaced(messages: &mut [(String, UnifiedInsiderTransaction)], earlier: &SeenCopy) {
    for ((_, m), name) in messages.iter_mut().zip(&earlier.names) {
        if m.person_name != *name {
            if let Some(model::generated::unified_insider_transaction::SourceDetail::Mar(d)) =
                m.source_detail.as_mut()
            {
                d.replaced_person_name = name.clone();
            }
        }
    }
}

/// The trade fields that must agree between two copies of one release: the
/// Kafka key (person and transaction date), volume, price, type and currency,
/// one entry per trade in order.
pub fn trades_identity(messages: &[(String, UnifiedInsiderTransaction)]) -> Vec<String> {
    messages
        .iter()
        .map(|(key, m)| {
            format!(
                "{key}|{}|{}|{}|{}|{}",
                m.transaction_date, m.volume, m.price_per_unit, m.transaction_type, m.currency
            )
        })
        .collect()
}

/// The Kafka key of a message, the convention of `pdmr.rs`:
/// `{SOURCE}:{TICKER}:{PERSON}:{DATE}`, the source from the message's registry.
pub fn insider_key(txn: &UnifiedInsiderTransaction) -> String {
    let source = if txn.source_registry == SourceRegistry::DkMar as i32 {
        DK_KEY_SOURCE
    } else {
        KEY_SOURCE
    };
    transaction_id(source, &txn.ticker, &txn.person_name, &txn.transaction_date)
}

/// Where insider messages go: Kafka in production, a vector in tests.
#[async_trait]
pub trait InsiderSink: Send + Sync {
    /// Delivers one message; returns once the broker acknowledged it.
    async fn send(&self, key: &str, txn: &UnifiedInsiderTransaction) -> anyhow::Result<()>;

    /// The topic messages go to, for the `send <topic>` span of `nordic-live`.
    fn destination(&self) -> &str {
        INSIDER_TRANSACTIONS_TOPIC
    }
}

/// Publishes to a Kafka topic.
pub struct KafkaInsiderSink {
    producer: FutureProducer,
    topic: String,
}

impl KafkaInsiderSink {
    /// Like [`KafkaInsiderSink::new`], tuned for latency and safety:
    /// `linger.ms` 5, `acks=all`, idempotent producer. Used by `nordic-live`.
    pub fn low_latency(brokers: &str, topic: impl Into<String>) -> anyhow::Result<Self> {
        let producer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "10000")
            .set("linger.ms", "5")
            .set("acks", "all")
            .set("enable.idempotence", "true")
            .create()?;
        Ok(Self {
            producer,
            topic: topic.into(),
        })
    }

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
        let headers = trace_headers();
        let mut record = FutureRecord::to(&self.topic).key(key).payload(&payload);
        if headers.count() > 0 {
            record = record.headers(headers);
        }
        let (partition, offset) = self
            .producer
            .send(record, Duration::from_secs(10))
            .await
            .map_err(|(e, _)| anyhow::anyhow!("publishing insider transaction {key}: {e}"))?;
        let span = Span::current();
        span.record("messaging.kafka.partition", i64::from(partition));
        span.record("messaging.kafka.offset", offset);
        Ok(())
    }

    fn destination(&self) -> &str {
        &self.topic
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
