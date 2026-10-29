//! Parser for Finnish MAR Art. 19 managers' transaction releases (NEX-206).
//!
//! Finnish issuers disclose manager trades as stock exchange releases that MFN
//! carries as text. This module turns that text into [`ParsedMarTransaction`]s.
//! It is pure: no I/O, Kafka or database code.
//!
//! Three layouts occur in the wild and are all handled:
//!
//! * a **box-drawn** table (Cision, e.g. Metso) with one `Label:  │value` row
//!   per field and a bare heading row such as `Initial Notification  │`;
//! * a **plain** layout (`Label: value` lines) where each transaction block
//!   starts at `Transaction date:` and lists `(n): Volume: .. Unit price: ..`
//!   lines (e.g. Hiab, Orion, Kone, Wärtsilä, Kesko);
//! * both in English and Finnish.
//!
//! Money is never parsed through `f64`; the exact [`Decimal`] is kept and only
//! converted to the legacy `double` fields at the proto boundary in
//! [`ParsedMarTransaction::into_unified`].
//!
//! Releases with a close associate (`Position: Closely associated person`) list
//! the manager in a following *Person Discharging Managerial Responsibilities*
//! block; the trade is attributed to that manager.

use chrono::NaiveDate;
use model::generated::{
    unified_insider_transaction, MarReleaseDetail, SourceRegistry, UnifiedInsiderTransaction,
    UnifiedTransactionType,
};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::str::FromStr;
use thiserror::Error;
use tracing::warn;

/// Prefix of the transaction id of every Finnish MAR row.
pub const ID_PREFIX: &str = "FIN_MAR";

/// Why a release could not be parsed. Variants name the missing label; nothing
/// is ever guessed, so the caller logs the error and skips the release.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MarParseError {
    /// `lang` was neither `en` nor `fi`.
    #[error("unsupported language {0:?} (expected \"en\" or \"fi\")")]
    UnsupportedLang(String),
    /// No `Name` / `Nimi` of the person subject to the notification.
    #[error("missing label: Name / Nimi")]
    MissingName,
    /// No `Position` / `Asema`.
    #[error("missing label: Position / Asema")]
    MissingPosition,
    /// Close-associate release without a manager block.
    #[error("missing block: Person Discharging Managerial Responsibilities / Liikkeeseenlaskijassa johtotehtävissä toimiva henkilö")]
    MissingManager,
    /// No `Reference number` / `Viitenumero`.
    #[error("missing label: Reference number / Viitenumero")]
    MissingReferenceNumber,
    /// No `Initial Notification` / `Amendment` marker.
    #[error("missing label: Notification type / Ilmoituksen luonne")]
    MissingNotificationType,
    /// No `Transaction date` / `Liiketoimen päivämäärä`, i.e. no transaction.
    #[error("missing label: Transaction date / Liiketoimen päivämäärä")]
    MissingTransactionDate,
    /// A transaction block without `Nature of the transaction`.
    #[error("missing label: Nature of the transaction / Liiketoimen luonne")]
    MissingNature,
    /// A transaction block without `Instrument` / `Instrumentti`.
    #[error("missing label: Instrument / Instrumentti")]
    MissingInstrument,
    /// A transaction block without any `Volume` / `Volyymi`.
    #[error("missing label: Volume / Volyymi")]
    MissingVolume,
    /// A volume without a `Unit price` / `Yksikköhinta`.
    #[error("missing label: Unit price / Yksikköhinta")]
    MissingUnitPrice,
    /// A date in no known format.
    #[error("invalid transaction date {0:?}")]
    InvalidDate(String),
    /// A volume or price that is not a positive/non-negative decimal.
    #[error("invalid number for {label}: {value:?}")]
    InvalidNumber {
        /// Label the value came from.
        label: &'static str,
        /// The offending text.
        value: String,
    },
    /// A unit price whose currency is unknown.
    #[error("unrecognised currency in unit price {0:?}")]
    InvalidCurrency(String),
    /// A unit price in a unit that is not a currency (e.g. `PCT` of par for debt).
    #[error("unit price {0:?} is not a per-unit amount in a currency")]
    UnsupportedPriceUnit(String),
    /// A zero price on an open-market BUY or SELL; never publish this as a trade.
    #[error("zero unit price on a {nature:?} transaction (index {index})")]
    SuspiciousZeroPrice {
        /// Position of the transaction within the release.
        index: usize,
        /// Raw nature text.
        nature: String,
    },
}

/// One transaction of a managers' transaction release.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedMarTransaction {
    /// Position within the release, starting at 0, across all blocks.
    pub index: usize,
    /// The manager the trade is attributed to, as "First Last".
    pub person_name: String,
    /// The manager's position as written.
    pub person_role: String,
    /// The trade was made by a person closely associated with the manager.
    pub is_close_associate: bool,
    /// Issuer name as written.
    pub issuer_name: String,
    /// Issuer LEI; empty when the release has none.
    pub lei: String,
    /// MAR notification reference number.
    pub reference_number: String,
    /// The notification amends an earlier one.
    pub is_amendment: bool,
    /// Free text explaining the amendment; empty for initial notifications.
    pub amendment_reason: String,
    /// Reference number of the amended notification, only if the text states it.
    pub original_reference_number: Option<String>,
    /// Execution date.
    pub transaction_date: NaiveDate,
    /// Trading venue as written; `None` when the release gives none.
    pub venue: Option<String>,
    /// Instrument type as written (`Share`, `Osake`).
    pub instrument_type: String,
    /// Instrument ISIN; empty when absent.
    pub isin: String,
    /// Raw nature text.
    pub nature: String,
    /// Classification of [`Self::nature`].
    pub transaction_type: UnifiedTransactionType,
    /// Number of units; may be fractional.
    pub volume: Decimal,
    /// Price per unit in [`Self::currency`]. Zero is normal for incentives.
    /// `None` when the release states `N/A`, which is accepted for non-BUY/SELL
    /// natures only (e.g. incentive receipts).
    pub unit_price: Option<Decimal>,
    /// ISO 4217 code; empty when the release states `N/A` for a zero price.
    pub currency: String,
    /// Language the text was parsed as.
    pub lang: String,
}

/// Where a release came from; the parser itself never sees this.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseContext<'a> {
    /// Uppercase ticker.
    pub ticker: &'a str,
    /// Exchange MIC.
    pub exchange_mic: &'a str,
    /// Release publication date, `YYYY-MM-DD`.
    pub published_date: &'a str,
    /// MFN news id.
    pub mfn_news_id: &'a str,
    /// MFN release url.
    pub url: &'a str,
}

impl ParsedMarTransaction {
    /// Stable id `FIN_MAR:{reference_number}:{index}`. A date-level id (as for
    /// SEC/FI) would collide: one person can trade several times a day.
    pub fn id(&self) -> String {
        format!("{ID_PREFIX}:{}:{}", self.reference_number, self.index)
    }

    /// Id of the amended row. Empty unless this is an amendment whose text
    /// states the original reference number; guessing would link the wrong row.
    pub fn amended_transaction_id(&self) -> String {
        match (&self.original_reference_number, self.is_amendment) {
            (Some(original), true) => format!("{ID_PREFIX}:{original}:{}", self.index),
            (None, true) => {
                warn!(
                    reference_number = %self.reference_number,
                    index = self.index,
                    "amendment does not state the original reference number; amended id left empty"
                );
                String::new()
            }
            _ => String::new(),
        }
    }

    /// Build the wire message. The shared `double` fields are filled from the
    /// exact decimals here and nowhere else.
    pub fn into_unified(self, ctx: ReleaseContext<'_>) -> UnifiedInsiderTransaction {
        let amended_transaction_id = self.amended_transaction_id();
        UnifiedInsiderTransaction {
            ticker: ctx.ticker.trim().to_uppercase(),
            exchange_mic: ctx.exchange_mic.to_string(),
            source_registry: SourceRegistry::FinMar as i32,
            person_name: self.person_name,
            person_role: self.person_role,
            transaction_date: self.transaction_date.format("%Y-%m-%d").to_string(),
            published_date: ctx.published_date.to_string(),
            transaction_type: self.transaction_type as i32,
            volume: self.volume.to_f64().unwrap_or_default(),
            price_per_unit: self.unit_price.and_then(|p| p.to_f64()).unwrap_or_default(),
            currency: self.currency,
            is_amendment: self.is_amendment,
            amended_transaction_id,
            source_detail: Some(unified_insider_transaction::SourceDetail::Mar(
                MarReleaseDetail {
                    lei: self.lei,
                    isin: self.isin,
                    reference_number: self.reference_number,
                    instrument_type: self.instrument_type,
                    venue: self.venue.unwrap_or_default(),
                    nature: self.nature,
                    is_close_associate: self.is_close_associate,
                    amendment_reason: self.amendment_reason,
                    mfn_news_id: ctx.mfn_news_id.to_string(),
                    url: ctx.url.to_string(),
                    lang: self.lang,
                    volume_decimal: self.volume.to_string(),
                    unit_price_decimal: self.unit_price.map(|p| p.to_string()).unwrap_or_default(),
                },
            )),
        }
    }
}

/// Parse the text of one managers' transaction release.
///
/// `lang` is the release language (`en` or `fi`); labels of both languages are
/// recognised either way. Returns one entry per transaction line; the
/// "Aggregated transactions" summary is never emitted.
pub fn parse(text: &str, lang: &str) -> Result<Vec<ParsedMarTransaction>, MarParseError> {
    let lang = lang.trim().to_lowercase();
    if lang != "en" && lang != "fi" {
        return Err(MarParseError::UnsupportedLang(lang));
    }
    let doc = scan(text)?;
    doc.finish(&lang)
}

/// Classify the raw "Nature of the transaction" text. Anything not recognised
/// is `OTHER`; only an explicit acquisition/disposal is ever BUY/SELL.
pub fn classify_nature(nature: &str) -> UnifiedTransactionType {
    use UnifiedTransactionType as T;
    let n = collapse(&nature.to_lowercase());
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    if has(&[
        "gift",
        "donation",
        "inheritance",
        "lahja",
        "lahjoitus",
        "perint",
    ]) {
        T::Gift
    } else if has(&["exercise", "toteuttaminen", "toteutus"]) {
        T::OptionExercise
    } else if has(&[
        "incentive",
        "remuneration",
        "palkkio",
        "grant",
        "award",
        "myöntäminen",
    ]) || (n.starts_with("acceptance") && n.contains("option"))
        || n.starts_with("osakeoption hyväksyminen")
    {
        T::Grant
    } else if has(&[
        "subscription",
        "merkintä",
        "pledg",
        "panttaus",
        "lending",
        "lainaus",
    ]) {
        T::Other
    } else if ["acquisition", "purchase", "hankinta", "osto"]
        .iter()
        .any(|w| n == *w || n.starts_with(&format!("{w} ")))
    {
        T::Buy
    } else if ["disposal", "sale", "luovutus", "myynti"]
        .iter()
        .any(|w| n == *w || n.starts_with(&format!("{w} ")))
    {
        T::Sell
    } else {
        T::Other
    }
}

/// "Takaluoma, Sami" -> "Sami Takaluoma". Names without exactly one comma are
/// kept as given; diacritics are untouched.
pub fn normalise_name(raw: &str) -> String {
    let raw = collapse(raw);
    let mut parts = raw.split(',');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(last), Some(first), None) if !last.trim().is_empty() && !first.trim().is_empty() => {
            format!("{} {}", first.trim(), last.trim())
        }
        _ => raw,
    }
}

/// Parse a decimal as written in releases: `.` or `,` as decimal separator,
/// spaces (incl. nbsp/thin) as thousands separators. Never goes through `f64`.
pub fn parse_decimal(raw: &str) -> Option<Decimal> {
    let s: String = raw.chars().filter(|c| !is_space(*c)).collect();
    if s.is_empty()
        || !s
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
    {
        return None;
    }
    // When both appear, the last one is the decimal separator; a lone `,` is a
    // decimal comma (Finnish).
    let (dots, commas) = (s.matches('.').count(), s.matches(',').count());
    let normalised = match (s.rfind('.'), s.rfind(',')) {
        // `14,182,052`: repeated separators can only be thousands separators.
        (None, Some(_)) if commas > 1 => s.replace(',', ""),
        (Some(_), None) if dots > 1 => s.replace('.', ""),
        (Some(d), Some(c)) if d > c => s.replace(',', ""),
        (Some(_), Some(_)) => s.replace('.', "").replace(',', "."),
        (None, Some(_)) => s.replace(',', "."),
        _ => s,
    };
    Decimal::from_str(&normalised).ok()
}

// ---------------------------------------------------------------------------
// Scanning

const BOX_CHARS: &str = "│|─┬┼┴├┤┌┐└┘═║╔╗╚╝_-= ";

fn is_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{00a0}' | '\u{2009}' | '\u{202f}' | '\u{200b}')
}

fn collapse(s: &str) -> String {
    s.split(is_space)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One logical line after layout normalisation.
#[derive(Debug)]
enum Item {
    Sep,
    /// A row without a value: a section heading or free text.
    Text(String),
    /// A recognisable `label: value` row.
    Field {
        label: String,
        value: String,
    },
    /// A box row with an empty label cell: continues the previous value.
    Continuation(String),
}

fn tokenise(text: &str) -> Vec<Item> {
    let mut items = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.chars().count() >= 3 && line.chars().all(|c| BOX_CHARS.contains(c)) {
            items.push(Item::Sep);
            continue;
        }
        // Box-drawn (`│`) and the markdown-style pipe table (`| Nimi: | Name |`)
        // older Cision releases use.
        let sep = if line.contains('│') {
            Some('│')
        } else if line.starts_with('|') {
            Some('|')
        } else {
            None
        };
        if let Some(sep) = sep {
            let cells: Vec<String> = line
                .trim_start_matches('|')
                .split(sep)
                .map(collapse)
                .collect();
            let label = cells[0].trim_end_matches(':').trim().to_string();
            let value = cells.get(1).cloned().unwrap_or_default();
            items.push(if label.is_empty() {
                if value.is_empty() {
                    continue;
                }
                Item::Continuation(value)
            } else if value.is_empty() && !cells[0].ends_with(':') {
                Item::Text(label)
            } else {
                Item::Field { label, value }
            });
            continue;
        }
        let line = collapse(line);
        let (line, _) = strip_enumeration(&line);
        if line.is_empty() {
            continue;
        }
        match line.split_once(':') {
            Some((label, value))
                if label.chars().count() <= 45
                    && !label.trim().is_empty()
                    && !label.contains("  ") =>
            {
                items.push(Item::Field {
                    label: label.trim().to_string(),
                    value: cut_checkbox(value).trim().to_string(),
                });
            }
            _ => items.push(Item::Text(cut_checkbox(line).trim().to_string())),
        }
    }
    items
}

/// Drop a leading `(X) Legal person ` checkbox and an `(n):` ordinal.
fn strip_enumeration(line: &str) -> (&str, bool) {
    let mut s = line.trim();
    if s.starts_with("(X)") || s.starts_with("(x)") {
        match find_ordinal(s) {
            Some(p) => s = &s[p..],
            None => return ("", false),
        }
    }
    match strip_ordinal(s) {
        Some(rest) => (rest.trim_start(), true),
        None => (s, false),
    }
}

fn find_ordinal(s: &str) -> Option<usize> {
    s.match_indices('(')
        .map(|(i, _)| i)
        .find(|&i| strip_ordinal(&s[i..]).is_some())
}

/// `(12):` or `(12)` at the start of `s`.
fn strip_ordinal(s: &str) -> Option<&str> {
    let rest = s.strip_prefix('(')?;
    let end = rest.find(')')?;
    if end == 0 || !rest[..end].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rest = &rest[end + 1..];
    Some(rest.strip_prefix(':').unwrap_or(rest))
}

/// Values may end in a checkbox such as `Closely associated person (X) Legal person`.
fn cut_checkbox(s: &str) -> &str {
    match s.find("(X)") {
        Some(i) => &s[..i],
        None => s,
    }
}

fn norm_label(label: &str) -> String {
    collapse(&label.to_lowercase())
        .trim_end_matches(':')
        .trim()
        .to_string()
}

fn label_is(label: &str, exact: &[&str], prefixes: &[&str]) -> bool {
    let l = norm_label(label);
    exact.iter().any(|e| l == *e) || prefixes.iter().any(|p| l.starts_with(p))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Section {
    Header,
    Person,
    Manager,
    Associate,
    Issuer,
    Transactions,
    End,
}

#[derive(Default)]
struct Block {
    date: Option<String>,
    venue: Option<String>,
    instrument: Option<String>,
    isin: String,
    nature: Option<String>,
    /// Raw `(volume, price)` texts; price is filled by `Unit price`.
    amounts: Vec<(Option<String>, Option<String>)>,
    aggregated: bool,
}

#[derive(Default)]
struct Doc {
    subject_name: Option<String>,
    subject_position: Option<String>,
    manager_name: Option<String>,
    manager_position: Option<String>,
    saw_manager_block: bool,
    saw_associate_block: bool,
    issuer_name: String,
    lei: String,
    reference: Option<String>,
    is_amendment: Option<bool>,
    amendment_text: String,
    blocks: Vec<Block>,
}

/// Which field the last value belongs to, for wrapped continuation rows.
#[derive(Clone, Copy, PartialEq)]
enum Last {
    None,
    Amendment,
    Other,
}

fn scan(text: &str) -> Result<Doc, MarParseError> {
    let mut doc = Doc::default();
    let mut section = Section::Header;
    let mut last = Last::None;

    for item in tokenise(text) {
        match item {
            Item::Sep => {
                last = Last::None;
            }
            Item::Continuation(value) => {
                if last == Last::Amendment {
                    push_text(&mut doc.amendment_text, &value);
                }
            }
            Item::Text(t) => {
                let l = norm_label(&t);
                if let Some(s) = heading_section(&l) {
                    section = s;
                    if s == Section::Manager {
                        doc.saw_manager_block = true;
                    }
                    if s == Section::Associate {
                        doc.saw_associate_block = true;
                    }
                    last = Last::None;
                } else if l.starts_with("transaction details")
                    || l.starts_with("liiketoimien yksityiskohtaiset")
                    || l.starts_with("liiketoimen tiedot")
                {
                    section = Section::Transactions;
                    last = Last::None;
                } else if l.starts_with("aggregated transactions")
                    || l.starts_with("liiketoimien yhdistetyt")
                {
                    if let Some(b) = doc.blocks.last_mut() {
                        b.aggregated = true;
                    }
                    last = Last::None;
                } else if let Some(kind) = notification_kind(&l) {
                    doc.is_amendment = Some(kind);
                    last = Last::None;
                } else if ends_content(&l) {
                    section = Section::End;
                    last = Last::None;
                } else if last == Last::Amendment {
                    push_text(&mut doc.amendment_text, &t);
                }
                // Anything else (`Venue not applicable`, `Outside a trading
                // venue`, footers) carries no field.
            }
            Item::Field { label, value } => {
                last = Last::Other;
                let l = norm_label(&label);
                if ends_content(&l) {
                    section = Section::End;
                    continue;
                }
                if section == Section::End {
                    continue;
                }
                // `Aggregated transactions (1):` tokenises as an empty field.
                if l.starts_with("aggregated transactions")
                    || l.starts_with("liiketoimien yhdistetyt")
                {
                    if let Some(b) = doc.blocks.last_mut() {
                        b.aggregated = true;
                    }
                    continue;
                }
                if l.starts_with("transaction details")
                    || l.starts_with("liiketoimien yksityiskohtaiset")
                {
                    continue;
                }
                if label_is(&l, &[], &["transaction date", "liiketoimen päivämäärä"]) {
                    section = Section::Transactions;
                    doc.blocks.push(Block {
                        date: Some(value),
                        ..Block::default()
                    });
                } else if let Some(heading) = heading_section(&l) {
                    section = heading;
                } else if label_is(&l, &["issuer", "liikkeeseenlaskija"], &[]) {
                    section = Section::Issuer;
                    if !value.is_empty() && doc.blocks.is_empty() {
                        doc.issuer_name = value;
                    }
                } else if label_is(&l, &["name", "nimi"], &[]) {
                    match section {
                        Section::Issuer => doc.issuer_name = value,
                        Section::Manager => {
                            doc.manager_name.get_or_insert(value);
                        }
                        Section::Person | Section::Header => {
                            doc.subject_name.get_or_insert(value);
                        }
                        _ => {}
                    }
                } else if label_is(&l, &["position", "asema"], &[]) {
                    match section {
                        Section::Manager => {
                            doc.manager_position.get_or_insert(value);
                        }
                        Section::Person | Section::Header => {
                            doc.subject_position.get_or_insert(value);
                        }
                        _ => {}
                    }
                } else if label_is(&l, &[], &["lei"]) && l == "lei" {
                    if doc.lei.is_empty() && doc.blocks.is_empty() {
                        doc.lei = value;
                    }
                } else if label_is(&l, &[], &["reference number", "viitenumero"]) {
                    if doc.reference.is_none() {
                        doc.reference = Some(value);
                    }
                } else if label_is(&l, &[], &["notification type", "ilmoituksen luonne"]) {
                    if let Some(kind) = notification_kind(&norm_label(&value)) {
                        doc.is_amendment = Some(kind);
                    }
                } else if label_is(
                    &l,
                    &[],
                    &[
                        "amendment comment",
                        "reason for amendment",
                        "amendment reason",
                        "muutoksen",
                        "korjauksen",
                    ],
                ) {
                    push_text(&mut doc.amendment_text, &value);
                    last = Last::Amendment;
                } else if let Some(block) = doc.blocks.last_mut() {
                    scan_block_field(block, &l, &value);
                }
            }
        }
    }
    Ok(doc)
}

fn push_text(buf: &mut String, add: &str) {
    if add.trim().is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push(' ');
    }
    buf.push_str(add.trim());
}

/// Section named by a heading row, if it is one.
fn heading_section(l: &str) -> Option<Section> {
    if l.starts_with("person subject to") || l.starts_with("ilmoitusvelvollinen") {
        Some(Section::Person)
    } else if l.starts_with("person discharging managerial")
        || l.starts_with("liikkeeseenlaskijassa johtotehtävissä")
    {
        Some(Section::Manager)
    } else if l.starts_with("person closely associated")
        || l.starts_with("closely associated person")
        || l == "lähipiiriin kuuluva henkilö"
        || l.starts_with("johtohenkilön lähipiiriin kuuluva")
    {
        Some(Section::Associate)
    } else if l == "issuer" || l == "liikkeeseenlaskija" {
        Some(Section::Issuer)
    } else {
        None
    }
}

/// `Some(true)` for an amendment marker, `Some(false)` for an initial one.
fn notification_kind(l: &str) -> Option<bool> {
    if l.starts_with("initial notification") || l.starts_with("ensimmäinen ilmoitus") {
        Some(false)
    } else if l.starts_with("amendment") || l.starts_with("muutos") || l.starts_with("korjaus") {
        Some(true)
    } else {
        None
    }
}

/// Headings after which only boilerplate follows.
fn ends_content(l: &str) -> bool {
    [
        "distribution",
        "jakelu",
        "further information",
        "for further information",
        "more information",
        "lisätietoja",
        "lisätiedot",
    ]
    .iter()
    .any(|p| l.starts_with(p))
}

fn scan_block_field(block: &mut Block, l: &str, value: &str) {
    if block.aggregated {
        return;
    }
    if label_is(l, &[], &["name of the instrument", "instrumentin nimi"]) {
        return;
    }
    if label_is(l, &[], &["trading venue", "venue", "kauppapaikka"]) {
        if !value.is_empty() {
            block.venue = Some(value.to_string());
        }
    } else if label_is(
        l,
        &[],
        &[
            "nature of the transaction",
            "nature of transaction",
            "liiketoimen luonne",
        ],
    ) {
        block.nature = Some(value.to_string());
    } else if label_is(
        l,
        &["instrument", "instrumentti"],
        &[
            "instrument type",
            "instrumenttityyppi",
            "instrumentti tyyppi",
        ],
    ) {
        block.instrument = Some(value.to_string());
    } else if l == "isin" {
        block.isin = value.to_string();
    } else if label_is(l, &[], &["volume weighted", "keskihinta"]) {
        // Summary of the block; never a trade.
    } else if label_is(l, &[], &["volume", "volyymi"]) {
        // A single row may carry both: `Volume: 5 Unit price: 1.0 EUR`.
        let (vol, price) = split_inline_price(value);
        block.amounts.push((Some(vol), price));
    } else if label_is(l, &[], &["unit price", "yksikköhinta"]) {
        match block.amounts.last_mut() {
            Some(last) if last.1.is_none() => last.1 = Some(value.to_string()),
            _ => block.amounts.push((None, Some(value.to_string()))),
        }
    }
}

/// Split `783 Unit price: 0 N/A` into the volume and the price text.
fn split_inline_price(value: &str) -> (String, Option<String>) {
    let lower = value.to_lowercase();
    for label in ["unit price", "yksikköhinta"] {
        if let Some(i) = lower.find(label) {
            let vol = value[..i].trim().to_string();
            let rest = value[i + label.len()..]
                .trim_start_matches([':', ' '])
                .trim();
            return (vol, Some(rest.to_string()));
        }
    }
    (value.trim().to_string(), None)
}

impl Doc {
    fn finish(self, lang: &str) -> Result<Vec<ParsedMarTransaction>, MarParseError> {
        let subject_name = self.subject_name.ok_or(MarParseError::MissingName)?;
        let subject_position = self
            .subject_position
            .ok_or(MarParseError::MissingPosition)?;
        let reference = self
            .reference
            .filter(|r| !r.is_empty())
            .ok_or(MarParseError::MissingReferenceNumber)?;
        let is_amendment = self
            .is_amendment
            .ok_or(MarParseError::MissingNotificationType)?;
        if self.blocks.is_empty() {
            return Err(MarParseError::MissingTransactionDate);
        }

        let subject_pos_lower = norm_label(&subject_position);
        let associate = subject_pos_lower.starts_with("closely associated")
            || subject_pos_lower.starts_with("lähipiiriin kuuluva")
            || self.saw_associate_block;
        let (person_name, person_role) = if associate {
            match (self.manager_name, self.manager_position) {
                (Some(n), Some(p)) => (normalise_name(&n), p),
                _ if self.saw_associate_block && !self.saw_manager_block => {
                    // Manager is the subject; the block names the associate.
                    (normalise_name(&subject_name), subject_position)
                }
                _ => return Err(MarParseError::MissingManager),
            }
        } else {
            (normalise_name(&subject_name), subject_position)
        };

        let original_reference_number = if is_amendment {
            original_reference(&self.amendment_text, &reference)
        } else {
            None
        };

        let mut out = Vec::new();
        for block in self.blocks {
            let date_raw = block.date.ok_or(MarParseError::MissingTransactionDate)?;
            let transaction_date = parse_date(&date_raw)?;
            let nature = block
                .nature
                .filter(|n| !n.is_empty())
                .ok_or(MarParseError::MissingNature)?;
            let instrument_type = block
                .instrument
                .filter(|n| !n.is_empty())
                .ok_or(MarParseError::MissingInstrument)?;
            let transaction_type = classify_nature(&nature);
            if block.amounts.is_empty() {
                return Err(MarParseError::MissingVolume);
            }
            for (vol, price) in block.amounts {
                let vol = vol.ok_or(MarParseError::MissingVolume)?;
                let price = price.ok_or(MarParseError::MissingUnitPrice)?;
                let volume = parse_decimal(&vol).filter(|v| !v.is_zero()).ok_or(
                    MarParseError::InvalidNumber {
                        label: "Volume",
                        value: vol,
                    },
                )?;
                let (unit_price, currency) = parse_price(&price)?;
                let index = out.len();
                if matches!(
                    transaction_type,
                    UnifiedTransactionType::Buy | UnifiedTransactionType::Sell
                ) {
                    match unit_price {
                        Some(p) if p.is_zero() => {
                            return Err(MarParseError::SuspiciousZeroPrice { index, nature })
                        }
                        Some(_) => {}
                        None => {
                            return Err(MarParseError::InvalidNumber {
                                label: "Unit price",
                                value: price,
                            })
                        }
                    }
                }
                out.push(ParsedMarTransaction {
                    index,
                    person_name: person_name.clone(),
                    person_role: person_role.clone(),
                    is_close_associate: associate,
                    issuer_name: self.issuer_name.clone(),
                    lei: self.lei.clone(),
                    reference_number: reference.clone(),
                    is_amendment,
                    amendment_reason: self.amendment_text.clone(),
                    original_reference_number: original_reference_number.clone(),
                    transaction_date,
                    venue: block.venue.clone(),
                    instrument_type: instrument_type.clone(),
                    isin: block.isin.clone(),
                    nature: nature.clone(),
                    transaction_type,
                    volume,
                    unit_price,
                    currency,
                    lang: lang.to_string(),
                });
            }
        }
        Ok(out)
    }
}

fn parse_date(raw: &str) -> Result<NaiveDate, MarParseError> {
    let raw = collapse(raw);
    // `2020-02-28+02:00`: the date with a zone offset.
    if let Some(d) = raw
        .get(..10)
        .and_then(|h| NaiveDate::parse_from_str(h, "%Y-%m-%d").ok())
    {
        return Ok(d);
    }
    ["%Y-%m-%d", "%d %B %Y", "%d.%m.%Y"]
        .iter()
        .find_map(|f| NaiveDate::parse_from_str(&raw, f).ok())
        .ok_or(MarParseError::InvalidDate(raw))
}

/// Parse `0.00 EUR`, `EUR 0.00`, `0,00 euroa`, `0 N/A`, and a bare `N/A`
/// (no price, `None`).
fn parse_price(raw: &str) -> Result<(Option<Decimal>, String), MarParseError> {
    let raw = collapse(raw);
    if matches!(raw.to_uppercase().as_str(), "N/A" | "NA") {
        return Ok((None, String::new()));
    }
    let mut number = String::new();
    let mut unit = String::new();
    for tok in raw.split(' ') {
        if tok
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
        {
            number.push_str(tok);
        } else {
            if !unit.is_empty() {
                unit.push(' ');
            }
            unit.push_str(tok);
        }
    }
    let price = parse_decimal(&number).ok_or(MarParseError::InvalidNumber {
        label: "Unit price",
        value: raw.clone(),
    })?;
    let currency = match unit.to_uppercase().as_str() {
        "EUR" | "EURO" | "EUROA" | "€" => "EUR".to_string(),
        "N/A" | "NA" | "-" if price.is_zero() => String::new(),
        "PCT" | "%" => return Err(MarParseError::UnsupportedPriceUnit(raw)),
        c if c.len() == 3 && c.chars().all(|ch| ch.is_ascii_alphabetic()) => c.to_string(),
        _ => return Err(MarParseError::InvalidCurrency(raw)),
    };
    Ok((Some(price), currency))
}

/// The reference number of the amended notification, if the text states one
/// (`... reference number 123/4/5 ...` / `viitenumero`). Dates alone, as in
/// "issued on 30 October 2023", do not identify a notification.
fn original_reference(text: &str, own: &str) -> Option<String> {
    let lower = text.to_lowercase();
    for key in ["reference number", "viitenumero"] {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(key) {
            let start = from + pos + key.len();
            let candidate = text[start..]
                .trim_start_matches([':', ' ', '"', '\''])
                .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ')' | '"'))
                .next()
                .unwrap_or("")
                .trim_end_matches('.');
            if candidate.chars().any(|c| c.is_ascii_digit()) && candidate != own {
                return Some(candidate.to_string());
            }
            from = start;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use UnifiedTransactionType as T;

    fn plain(nature: &str, vol: &str, price: &str) -> String {
        format!(
            "Person subject to the notification requirement\nName: Doe, Jane\nPosition: CFO\n\
             Issuer: Acme Oyj\nLEI: 123\nNotification type: INITIAL NOTIFICATION\n\
             Reference number: R1\n____\nTransaction date: 2026-01-02\nVenue: XHEL\n\
             Instrument type: SHARE\nISIN: FI0000000001\nNature of transaction: {nature}\n\
             Transaction details\n(1): Volume: {vol} Unit price: {price}\n\
             Aggregated transactions (1):\nVolume: {vol} Volume weighted average price: {price}\n"
        )
    }

    #[test]
    fn nature_mapping_has_a_row_per_category() {
        let rows = [
            ("Acquisition", T::Buy),
            ("ACQUISITION", T::Buy),
            ("Hankinta", T::Buy),
            ("Disposal", T::Sell),
            ("Luovutus", T::Sell),
            ("Receipt of a Share-Based Incentive", T::Grant),
            ("Receipt of remuneration", T::Grant),
            ("Osakepalkkion vastaanottaminen", T::Grant),
            ("Acceptance of a stock option", T::Grant),
            ("Osakeoption hyväksyminen", T::Grant),
            ("Subscription", T::Other),
            ("Merkintä", T::Other),
            ("Exercise of a stock option", T::OptionExercise),
            ("Osakeoption toteuttaminen", T::OptionExercise),
            ("Gift", T::Gift),
            ("Receipt of a gift", T::Gift),
            ("Gift, donation or inheritance (received)", T::Gift),
            ("Lahja", T::Gift),
            ("Pledging", T::Other),
            ("Panttaus", T::Other),
            ("Redemption of pledge", T::Other),
            ("Lending", T::Other),
            ("Other", T::Other),
            ("Muu", T::Other),
            ("Something unheard of", T::Other),
        ];
        for (raw, want) in rows {
            assert_eq!(classify_nature(raw), want, "{raw}");
        }
    }

    #[test]
    fn incentive_is_never_a_buy() {
        assert_ne!(
            classify_nature("Receipt of a share-based incentive"),
            T::Buy
        );
    }

    #[test]
    fn names_are_normalised() {
        assert_eq!(normalise_name("Takaluoma, Sami"), "Sami Takaluoma");
        assert_eq!(normalise_name("Pakalén, Niko"), "Niko Pakalén");
        assert_eq!(normalise_name("Koipijärvi, Terhi"), "Terhi Koipijärvi");
        assert_eq!(normalise_name("Sami Takaluoma"), "Sami Takaluoma");
        assert_eq!(normalise_name("Foo, Bar, Baz"), "Foo, Bar, Baz");
    }

    #[test]
    fn decimals() {
        assert_eq!(parse_decimal("10,11110").unwrap().to_string(), "10.11110");
        assert_eq!(parse_decimal("13 000").unwrap().to_string(), "13000");
        assert_eq!(parse_decimal("4\u{a0}267").unwrap().to_string(), "4267");
        assert_eq!(
            parse_decimal("1\u{202f}743,5").unwrap().to_string(),
            "1743.5"
        );
        assert_eq!(parse_decimal("1,234.50").unwrap().to_string(), "1234.50");
        assert_eq!(parse_decimal("0.00").unwrap().to_string(), "0.00");
        assert_eq!(parse_decimal("14,182,052").unwrap().to_string(), "14182052");
        assert_eq!(parse_decimal("1.234.567").unwrap().to_string(), "1234567");
        assert!(parse_decimal("12x").is_none());
        assert!(parse_decimal("").is_none());
    }

    #[test]
    fn zero_price_is_valid_for_incentives_and_rejected_for_buys() {
        let ok = parse(
            &plain("RECEIPT OF A SHARE-BASED INCENTIVE", "810", "0.00 EUR"),
            "en",
        )
        .unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].transaction_type, T::Grant);
        assert_eq!(ok[0].unit_price, Some(Decimal::ZERO));
        for nature in ["ACQUISITION", "DISPOSAL"] {
            let err = parse(&plain(nature, "810", "0.00 EUR"), "en").unwrap_err();
            assert!(
                matches!(err, MarParseError::SuspiciousZeroPrice { index: 0, .. }),
                "{err:?}"
            );
        }
    }

    #[test]
    fn price_formats() {
        for (raw, price, cur) in [
            ("EUR 0.00", "0.00", "EUR"),
            ("0,00 euroa", "0.00", "EUR"),
            ("0 N/A", "0", ""),
            ("12,34 USD", "12.34", "USD"),
        ] {
            let (p, c) = parse_price(raw).unwrap();
            assert_eq!(
                (p.unwrap().to_string().as_str(), c.as_str()),
                (price, cur),
                "{raw}"
            );
        }
        assert!(matches!(
            parse_price("99.594 PCT"),
            Err(MarParseError::UnsupportedPriceUnit(_))
        ));
        assert!(matches!(
            parse_price("5 N/A"),
            Err(MarParseError::InvalidCurrency(_))
        ));
        assert!(matches!(
            parse_price("12.0876"),
            Err(MarParseError::InvalidCurrency(_))
        ));
        assert_eq!(parse_price("N/A").unwrap(), (None, String::new()));
    }

    #[test]
    fn na_price_is_accepted_for_incentives_only() {
        let ok = parse(
            &plain("RECEIPT OF A SHARE-BASED INCENTIVE", "783", "N/A"),
            "en",
        )
        .unwrap();
        assert_eq!(ok[0].unit_price, None);
        let msg = ok[0].clone().into_unified(ReleaseContext {
            ticker: "X",
            exchange_mic: "XHEL",
            published_date: "2026-01-03",
            mfn_news_id: "n",
            url: "u",
        });
        assert_eq!(msg.price_per_unit, 0.0);
        let Some(unified_insider_transaction::SourceDetail::Mar(d)) = msg.source_detail else {
            panic!()
        };
        assert_eq!(
            d.unit_price_decimal, "",
            "unknown price is not published as \"0\""
        );
        let err = parse(&plain("ACQUISITION", "783", "N/A"), "en").unwrap_err();
        assert!(
            matches!(err, MarParseError::InvalidNumber { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn fractional_volume_is_exact() {
        let tx = parse(&plain("ACQUISITION", "12,5", "1.10 EUR"), "en").unwrap();
        assert_eq!(tx[0].volume.to_string(), "12.5");
    }

    #[test]
    fn aggregate_is_not_a_trade_and_lines_get_indices() {
        let text = plain("ACQUISITION", "10", "2.00 EUR").replace(
            "(1): Volume: 10 Unit price: 2.00 EUR\n",
            "(1): Volume: 10 Unit price: 2.00 EUR\n(2): Volume: 5 Unit price: 3.00 EUR\n",
        );
        let tx = parse(&text, "en").unwrap();
        assert_eq!(tx.len(), 2);
        assert_eq!(tx[1].index, 1);
        assert_eq!(tx[1].id(), "FIN_MAR:R1:1");
    }

    #[test]
    fn errors_name_the_missing_label() {
        assert_eq!(
            parse("hello", "en").unwrap_err(),
            MarParseError::MissingName
        );
        assert!(parse("hello", "sv").is_err());
        let no_ref = plain("ACQUISITION", "1", "1 EUR").replace("Reference number: R1\n", "");
        let err = parse(&no_ref, "en").unwrap_err();
        assert_eq!(err, MarParseError::MissingReferenceNumber);
        assert!(err.to_string().contains("Reference number"));
        let no_nature =
            plain("ACQUISITION", "1", "1 EUR").replace("Nature of transaction: ACQUISITION\n", "");
        assert_eq!(
            parse(&no_nature, "en").unwrap_err(),
            MarParseError::MissingNature
        );
        let no_price = plain("ACQUISITION", "1", "1 EUR")
            .replace(" Unit price: 1 EUR\nAggregated", "\nAggregated");
        assert_eq!(
            parse(&no_price, "en").unwrap_err(),
            MarParseError::MissingUnitPrice
        );
    }

    #[test]
    fn amendment_id_only_when_original_reference_is_stated() {
        let base = plain("ACQUISITION", "1", "1 EUR").replace("INITIAL NOTIFICATION", "AMENDMENT");
        let none = parse(&base, "en").unwrap();
        assert!(none[0].is_amendment);
        assert_eq!(none[0].amended_transaction_id(), "");

        let with_text = base.replace(
            "Reference number: R1\n",
            "Reference number: R1\nAmendment comment:\nAmends the notification with reference number 41890/11/15 issued earlier.\n",
        );
        let some = parse(&with_text, "en").unwrap();
        assert_eq!(
            some[0].original_reference_number.as_deref(),
            Some("41890/11/15")
        );
        assert_eq!(some[0].amended_transaction_id(), "FIN_MAR:41890/11/15:0");
        assert_eq!(some[0].id(), "FIN_MAR:R1:0");
    }

    #[test]
    fn into_unified_fills_doubles_from_decimals() {
        let tx = parse(&plain("ACQUISITION", "12,5", "30.44 EUR"), "en")
            .unwrap()
            .remove(0);
        let msg = tx.into_unified(ReleaseContext {
            ticker: "acme",
            exchange_mic: "XHEL",
            published_date: "2026-01-03",
            mfn_news_id: "n1",
            url: "https://mfn.se/x",
        });
        assert_eq!(msg.ticker, "ACME");
        assert_eq!(msg.source_registry, SourceRegistry::FinMar as i32);
        assert_eq!(msg.volume, 12.5);
        assert_eq!(msg.price_per_unit, 30.44);
        assert_eq!(msg.currency, "EUR");
        assert_eq!(msg.person_name, "Jane Doe");
        let Some(unified_insider_transaction::SourceDetail::Mar(d)) = msg.source_detail else {
            panic!("expected MarReleaseDetail");
        };
        assert_eq!(d.volume_decimal, "12.5");
        assert_eq!(d.unit_price_decimal, "30.44");
        assert_eq!(d.reference_number, "R1");
        assert_eq!(d.lang, "en");
    }
}
