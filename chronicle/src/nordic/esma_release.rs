//! Parser for managers' transaction releases in ESMA's notification template
//! (Commission Implementing Regulation (EU) 2016/523), as Danish issuers
//! publish them (NEX-282).
//!
//! The template has four numbered sections: the person (1), the reason
//! (2: position, initial notification or amendment), the issuer (3: name, LEI)
//! and the transactions (4: instrument and identification code, nature,
//! price(s) and volume(s), aggregated information, date, place). Issuers write
//! it in the local language, in English, or both, as
//!
//! * a box-drawn table (`│`), with or without a column of section numbers and
//!   letters (`1`, `a)`, `4.1`), the price/volume pairs either inside the row or
//!   in a sub-table of their own;
//! * a markdown-style pipe table (`| a) | Name | ... |`);
//! * plain lines (`a) Navn: ...`), the pairs in a small box table.
//!
//! All three become the same stream of fields, matched by label in the
//! languages of the issuer's country ([`model::nordic::Country::languages`]).
//! A release may hold several forms (one per person) and a form several
//! transaction blocks. Numbers are read with the decimal separator of the
//! form's language (the language whose labels matched), never through `f64`.
//! Each block's price/volume pairs must add up to its stated aggregated volume,
//! else the release is refused: a misread separator must not become a trade.
//!
//! Nothing is guessed: a release without the form in its text (the form is a
//! PDF attachment) is [`MarParseError::FormNotInText`], one in the older Danish
//! short form (number and market value, no unit price) is
//! [`MarParseError::UnsupportedForm`], and a form missing a field names it.

use super::classify::strip_issuer_prefix;
use super::feed::NewsItem;
use super::lang::report_language;
use super::mar_release::{normalise_name, MarParseError, ParsedMarTransaction};
use chrono::NaiveDate;
use model::generated::UnifiedTransactionType;
use model::nordic::Lang;
use rust_decimal::Decimal;
use std::str::FromStr;

/// One field or heading of the template.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    PersonHeading,
    ReasonHeading,
    IssuerHeading,
    TransactionHeading,
    Name,
    Position,
    Notification,
    Lei,
    Instrument,
    Identification,
    Nature,
    Prices,
    Aggregated,
    Date,
    Place,
}

impl Field {
    fn is_heading(self) -> bool {
        matches!(
            self,
            Field::PersonHeading
                | Field::ReasonHeading
                | Field::IssuerHeading
                | Field::TransactionHeading
        )
    }

    /// Fields whose value can continue in a later table or after a blank line
    /// (the plain layout puts them in a small box table of their own).
    /// Fields whose label may stand alone on a plain line, without a colon.
    fn stands_alone(self) -> bool {
        self.is_heading() || matches!(self, Field::Instrument | Field::Prices | Field::Aggregated)
    }

    fn spans_tables(self) -> bool {
        matches!(
            self,
            Field::Prices | Field::Instrument | Field::Identification
        )
    }
}

/// The words of one language, all lower case.
struct Vocabulary {
    lang: Lang,
    /// `,` is the decimal separator and `.` groups thousands.
    decimal_comma: bool,
    /// `(label prefix, field)`, matched against the start of a label.
    labels: &'static [(&'static str, Field)],
    /// Headline phrases of a managers' transaction release.
    headlines: &'static [&'static str],
    /// Phrases of the form itself, for a release whose headline says nothing.
    form_text: &'static [&'static str],
    /// Headline phrases of releases that are never managers' transactions.
    excluded: &'static [&'static str],
    /// Notification values meaning an amendment.
    amendment: &'static [&'static str],
    /// Notification values meaning an initial notification.
    initial: &'static [&'static str],
    /// Position wording of a person closely associated with a manager.
    close_associate: &'static [&'static str],
    /// Labels after which only boilerplate follows.
    content_end: &'static [&'static str],
    /// Lines of the price field that are summaries, never a pair.
    price_summary: &'static [&'static str],
    /// Words of the older short form (number and market value).
    short_form: &'static [&'static str],
    /// Words saying the price/volume pairs are in an attachment.
    see_attachment: &'static [&'static str],
    natures: Natures,
}

/// Nature words by transaction type; `buy` and `sell` must start the nature.
struct Natures {
    gift: &'static [&'static str],
    exercise: &'static [&'static str],
    grant: &'static [&'static str],
    other: &'static [&'static str],
    buy: &'static [&'static str],
    sell: &'static [&'static str],
}

const ENGLISH: Vocabulary = Vocabulary {
    lang: Lang::En,
    decimal_comma: false,
    labels: &[
        ("details of the person", Field::PersonHeading),
        ("details of the reporting person", Field::PersonHeading),
        ("reason for the notification", Field::ReasonHeading),
        ("details of the issuer", Field::IssuerHeading),
        ("details of the transaction", Field::TransactionHeading),
        ("name", Field::Name),
        ("position", Field::Position),
        ("initial notification", Field::Notification),
        ("initial", Field::Notification),
        ("lei", Field::Lei),
        ("description of the financial", Field::Instrument),
        ("description of the instrument", Field::Instrument),
        ("identification code", Field::Identification),
        ("nature of the", Field::Nature),
        ("nature of transaction", Field::Nature),
        ("price(s)", Field::Prices),
        ("prices and volumes", Field::Prices),
        ("price and volume", Field::Prices),
        ("aggregated", Field::Aggregated),
        ("date of the transaction", Field::Date),
        ("date of transaction", Field::Date),
        ("place of the", Field::Place),
        ("place of transaction", Field::Place),
    ],
    headlines: &[
        "managers' transaction",
        "managers\u{2019} transaction",
        "managerial responsibilit",
        "managerial employees",
        "managerial staff",
        "board members, executives and associated persons",
        "executive's transaction",
        "executive\u{2019}s transaction",
        "executives' transaction",
        "executives\u{2019} transaction",
        "transactions of executives",
        "transactions by executive",
        "by executive and closely",
        "by directors and executive officers",
        "by managers and closely associated",
        "insiders trade",
        "insider's trade",
        "insiders' trade",
        "pdmr",
    ],
    form_text: &[
        "details of the person discharging managerial responsibilities",
        "details of the reporting person",
    ],
    excluded: &[
        "buy-back",
        "buyback",
        "buy back",
        "share repurchase",
        "repurchase programme",
        "repurchase program",
        "major shareholder",
        "section 38",
        "treasury share",
        "own shares",
    ],
    amendment: &["amendment", "correction", "amended"],
    initial: &["initial", "first"],
    close_associate: &[
        "closely associated",
        "closely related",
        "related party",
        "related person",
    ],
    content_end: &[
        "further information",
        "for further information",
        "for more information",
        "contact",
    ],
    price_summary: &[
        "minimum",
        "maximum",
        "lowest",
        "highest",
        "average",
        "weighted",
        "aggregated",
        "total",
    ],
    short_form: &["market value", "number:", "amount:"],
    see_attachment: &[
        "see separate",
        "see attach",
        "see appendix",
        "exhibit",
        "attached",
    ],
    natures: Natures {
        gift: &["gift", "donation", "inheritance"],
        exercise: &["exercise"],
        grant: &[
            "grant",
            "award",
            "allocation",
            "allotment",
            "vesting",
            "vested",
            "incentive",
            "share-based",
            "restricted share",
            "matching share",
            "performance share",
            "remuneration",
        ],
        other: &[
            "subscription",
            "pledge",
            "lending",
            "loan",
            "transfer",
            "conversion",
        ],
        buy: &["purchase", "acquisition", "buy", "bought"],
        sell: &["sale", "sell", "sold", "disposal"],
    },
};

const DANISH: Vocabulary = Vocabulary {
    lang: Lang::Da,
    decimal_comma: true,
    labels: &[
        ("nærmere oplysninger om personen", Field::PersonHeading),
        ("oplysninger om personen", Field::PersonHeading),
        ("oplysninger om den person", Field::PersonHeading),
        ("årsag til indberetning", Field::ReasonHeading),
        ("nærmere oplysninger om udstederen", Field::IssuerHeading),
        ("oplysninger om udstederen", Field::IssuerHeading),
        (
            "nærmere oplysninger om transaktion",
            Field::TransactionHeading,
        ),
        ("oplysninger om transaktion", Field::TransactionHeading),
        ("navn", Field::Name),
        ("stilling", Field::Position),
        ("første indberetning", Field::Notification),
        ("første", Field::Notification),
        ("lei", Field::Lei),
        ("beskrivelse af det finansielle", Field::Instrument),
        ("beskrivelse af instrumentet", Field::Instrument),
        ("identifikationskode", Field::Identification),
        ("transaktionens art", Field::Nature),
        ("transaktionens karakter", Field::Nature),
        ("transaktionstype", Field::Nature),
        ("pris(er)", Field::Prices),
        ("kurs(er)", Field::Prices),
        ("pris og mængde", Field::Prices),
        ("aggregere", Field::Aggregated),
        ("dato for transaktionen", Field::Date),
        ("transaktionsdato", Field::Date),
        ("sted for transaktionen", Field::Place),
        ("handelsplads", Field::Place),
    ],
    headlines: &[
        "ledende medarbejder",
        "ledelsesansvar",
        "nærtståendes transaktion",
        "nærtstående personers handel",
        "nærtståendes handel",
        "insideres handel",
    ],
    form_text: &[
        "oplysninger om personen med ledelsesansvar",
        "personer med ledelsesansvar",
    ],
    excluded: &[
        "tilbagekøb",
        "aktietilbagekøb",
        "storaktionær",
        "§ 38",
        "§38",
        "egne aktier",
    ],
    amendment: &["ændring", "korrektion", "rettelse"],
    initial: &["første"],
    close_associate: &["nærtstående", "nær tilknytning", "nært knyttet"],
    content_end: &[
        "yderligere information",
        "yderligere oplysninger",
        "for yderligere",
        "kontakt",
    ],
    price_summary: &[
        "minimum",
        "maksimum",
        "laveste",
        "højeste",
        "gennemsnit",
        "vægtet",
        "aggregere",
        "i alt",
    ],
    short_form: &["markedsværdi", "kursværdi", "antal:", "antal stk"],
    see_attachment: &["se vedhæft", "se bilag", "vedhæftede", "se særskilt"],
    natures: Natures {
        gift: &["gave", "arv", "donation"],
        exercise: &["udnyttelse"],
        grant: &[
            "tildeling",
            "allokering",
            "modning",
            "incitament",
            "aktieløn",
            "aktiebaseret",
            "betingede aktier",
            "matching",
        ],
        other: &["tegning", "pant", "udlån", "overførsel", "konvertering"],
        buy: &["køb", "erhvervelse", "tilkøb"],
        sell: &["salg", "afhændelse", "frasalg"],
    },
};

/// The vocabulary of `lang`, where the template is known in it.
fn vocabulary(lang: Lang) -> Option<&'static Vocabulary> {
    match lang {
        Lang::En => Some(&ENGLISH),
        Lang::Da => Some(&DANISH),
        Lang::Sv | Lang::Fi => None,
    }
}

fn vocabularies(langs: &[Lang]) -> Vec<&'static Vocabulary> {
    langs.iter().filter_map(|l| vocabulary(*l)).collect()
}

/// Currency codes accepted next to a price.
const CURRENCIES: &[&str] = &["DKK", "EUR", "USD", "SEK", "NOK", "GBP", "CHF"];

fn is_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{00a0}' | '\u{2009}' | '\u{202f}' | '\u{200b}')
}

fn collapse(s: &str) -> String {
    s.split(is_space)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn norm(s: &str) -> String {
    collapse(&s.to_lowercase())
        .trim_start_matches(['*', '-', '\u{2022}', ' '])
        .trim_end_matches(':')
        .trim()
        .to_string()
}

/// True if `item` is a managers' transaction release in the vocabularies of
/// `langs`: the headline (or the form in the text) says so, and the headline
/// names no share buyback, major shareholder or treasury-share notice.
pub fn is_managers_transaction(item: &NewsItem, langs: &[Lang]) -> bool {
    let vocabs = vocabularies(langs);
    let full = item.title.to_lowercase();
    let title = strip_issuer_prefix(&item.title).to_lowercase();
    if vocabs
        .iter()
        .flat_map(|v| v.excluded.iter())
        .any(|p| full.contains(p))
    {
        return false;
    }
    if vocabs
        .iter()
        .flat_map(|v| v.headlines.iter())
        .any(|p| title.contains(p) || full.contains(p))
    {
        return true;
    }
    item.text.as_deref().is_some_and(|t| {
        let t = t.to_lowercase();
        vocabs
            .iter()
            .flat_map(|v| v.form_text.iter())
            .any(|p| t.contains(p))
    })
}

// ---------------------------------------------------------------------------
// Lines and rows

const BOX_CHARS: &str = "│|─┬┼┴├┤┌┐└┘═║╔╗╚╝+_-= ";

#[derive(Debug)]
enum Line {
    Blank,
    Sep(usize),
    Table { block: usize, cells: Vec<String> },
    Plain(String),
}

fn is_table_line(line: &str) -> bool {
    line.contains('│') || line.trim_start().starts_with('|')
}

/// The cells of a table line. Only a separator in the first column is an outer
/// border: `  │value` is an empty cell followed by `value`.
fn split_cells(raw: &str) -> Vec<String> {
    let sep = if raw.contains('│') { '│' } else { '|' };
    let line = raw.trim_end();
    let line = if sep == '|' { line.trim_start() } else { line };
    let trimmed = line.strip_prefix(sep).unwrap_or(line);
    let trimmed = trimmed.strip_suffix(sep).unwrap_or(trimmed);
    let mut cells: Vec<String> = trimmed.split(sep).map(collapse).collect();
    while cells.len() > 1 && cells.last().is_some_and(|c| c.is_empty()) {
        cells.pop();
    }
    cells
}

fn lines(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut block = 0;
    let mut in_table = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            out.push(Line::Blank);
            in_table = false;
            continue;
        }
        let rule = line.chars().count() >= 2
            && line.chars().all(|c| BOX_CHARS.contains(c))
            && line.chars().any(|c| "─-=_═┬┼┴+".contains(c));
        if rule || is_table_line(line) {
            if !in_table {
                block += 1;
                in_table = true;
            }
            out.push(if rule {
                Line::Sep(block)
            } else {
                Line::Table {
                    block,
                    cells: split_cells(raw),
                }
            });
            continue;
        }
        in_table = false;
        out.push(Line::Plain(collapse(line)));
    }
    out
}

/// `1`, `1.`, `4.1`, `a)`, `3)`: a section number or letter.
fn is_index(cell: &str) -> bool {
    let c = cell.trim();
    if let Some(body) = c.strip_suffix(')') {
        return (body.len() == 1 && body.chars().all(|ch| ch.is_ascii_lowercase()))
            || (!body.is_empty() && body.len() <= 2 && body.chars().all(|ch| ch.is_ascii_digit()));
    }
    let c = c.trim_end_matches('.');
    !c.is_empty()
        && c.split('.').all(|part| {
            !part.is_empty() && part.len() <= 2 && part.chars().all(|ch| ch.is_ascii_digit())
        })
}

/// Strip a leading `a) `, `1 `, `4.1 `, `* `, `- ` from a plain line.
fn strip_plain_index(line: &str) -> &str {
    let mut s = line.trim();
    if let Some((head, rest)) = s.split_once(' ') {
        if is_index(head) && rest.chars().next().is_some_and(|c| c.is_alphabetic()) {
            s = rest.trim_start();
        }
    }
    s.trim_start_matches(['*', '-', '\u{2022}', ' '])
}

struct Matcher<'a> {
    vocabs: &'a [&'static Vocabulary],
}

impl Matcher<'_> {
    /// The field a label starts, with the vocabulary it matched in. A label
    /// wrapped inside a narrow column (`Descriptio` / `n of the`) still
    /// matches a long prefix: those are compared without spaces.
    fn field(&self, label: &str) -> Option<(Field, &'static Vocabulary)> {
        let l = norm(label);
        if l.is_empty() {
            return None;
        }
        let compact: String = l.chars().filter(|c| !c.is_whitespace()).collect();
        let mut best: Option<(usize, Field, &'static Vocabulary)> = None;
        for v in self.vocabs {
            for (prefix, field) in v.labels {
                let bounded = l.starts_with(prefix)
                    && l[prefix.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| !c.is_alphanumeric() || prefix.len() > 6);
                let compact_prefix: String =
                    prefix.chars().filter(|c| !c.is_whitespace()).collect();
                let wrapped = prefix.len() > 6 && compact.starts_with(&compact_prefix);
                if (bounded || wrapped) && best.is_none_or(|(len, _, _)| prefix.len() > len) {
                    best = Some((prefix.len(), *field, v));
                }
            }
        }
        best.map(|(_, f, v)| (f, v))
    }

    fn ends_content(&self, label: &str) -> bool {
        let l = norm(label);
        self.vocabs
            .iter()
            .flat_map(|v| v.content_end.iter())
            .any(|p| l.starts_with(p))
    }
}

/// A field as read: its kind, the vocabulary its label matched, and the cells
/// of each value line.
#[derive(Debug)]
struct Value {
    field: Field,
    lang: Lang,
    lines: Vec<Vec<String>>,
}

impl Value {
    fn text(&self) -> String {
        collapse(
            &self
                .lines
                .iter()
                .map(|l| l.join(" "))
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    fn first_line(&self) -> String {
        self.lines
            .iter()
            .map(|l| collapse(&l.join(" ")))
            .find(|l| !l.is_empty())
            .unwrap_or_default()
    }
}

/// One table row: in a box table the lines between two rules, else one line.
/// Section numbers and letters are already removed.
struct Row {
    block: usize,
    /// The label cells joined, when the table has a label column.
    label: Option<String>,
    /// The value cells of each line (everything but the label cell).
    values: Vec<Vec<String>>,
    /// Every cell of each line.
    cells: Vec<Vec<String>>,
}

enum Unit {
    Blank,
    Row(Row),
    Plain(String),
}

fn units(text: &str, m: &Matcher<'_>) -> Vec<Unit> {
    let all = lines(text);
    let mut indexed = std::collections::HashSet::<usize>::new();
    let mut boxed = std::collections::HashSet::<usize>::new();
    for line in &all {
        match line {
            Line::Table { block, cells } if cells.len() > 1 && is_index(&cells[0]) => {
                indexed.insert(*block);
            }
            Line::Sep(block) => {
                boxed.insert(*block);
            }
            _ => {}
        }
    }
    let mut raw: Vec<(usize, Vec<Vec<String>>)> = Vec::new();
    let mut out_order: Vec<Option<usize>> = Vec::new();
    let mut plain: Vec<Unit> = Vec::new();
    let mut open: Option<usize> = None;
    for line in all {
        match line {
            Line::Table { block, mut cells } => {
                if indexed.contains(&block)
                    && !cells.is_empty()
                    && (cells[0].is_empty() || is_index(&cells[0]))
                {
                    cells.remove(0);
                }
                let joins = boxed.contains(&block) && open.is_some_and(|i| raw[i].0 == block);
                if joins {
                    if let Some(i) = open {
                        raw[i].1.push(cells);
                    }
                } else {
                    raw.push((block, vec![cells]));
                    out_order.push(Some(raw.len() - 1));
                    open = boxed.contains(&block).then_some(raw.len() - 1);
                }
            }
            Line::Sep(_) => open = None,
            Line::Blank => {
                open = None;
                plain.push(Unit::Blank);
                out_order.push(None);
            }
            Line::Plain(t) => {
                open = None;
                plain.push(Unit::Plain(t));
                out_order.push(None);
            }
        }
    }
    let label_of = |lines: &[Vec<String>]| {
        collapse(
            &lines
                .iter()
                .filter_map(|l| l.first())
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    let labelled: std::collections::HashSet<usize> = raw
        .iter()
        .filter(|(_, lines)| m.field(&label_of(lines)).is_some())
        .map(|(b, _)| *b)
        .collect();
    let mut plain = plain.into_iter();
    let mut rows = raw.into_iter();
    out_order
        .into_iter()
        .filter_map(|slot| match slot {
            None => plain.next(),
            Some(_) => rows.next().map(|(block, lines)| {
                let has_label = labelled.contains(&block);
                let label = has_label.then(|| label_of(&lines));
                let values = lines
                    .iter()
                    .map(|l| {
                        if has_label {
                            l.get(1..).map(<[String]>::to_vec).unwrap_or_default()
                        } else {
                            l.clone()
                        }
                    })
                    .collect();
                Unit::Row(Row {
                    block,
                    label,
                    values,
                    cells: lines,
                })
            }),
        })
        .collect()
}

/// Turns the text into fields. Content after an end-of-content label is dropped.
fn fields(text: &str, m: &Matcher<'_>) -> Vec<Value> {
    struct Open {
        value: Value,
        block: Option<usize>,
        blank: bool,
    }
    let mut out: Vec<Value> = Vec::new();
    let mut current: Option<Open> = None;
    let mut after_index = false;
    let flush = |current: &mut Option<Open>, out: &mut Vec<Value>| {
        if let Some(o) = current.take() {
            out.push(o.value);
        }
    };
    for unit in units(text, m) {
        match unit {
            Unit::Blank => {
                if let Some(o) = current.as_mut() {
                    o.blank = true;
                }
            }
            Unit::Row(row) => {
                after_index = false;
                if row.cells.iter().flatten().all(|c| c.is_empty()) {
                    continue;
                }
                if row.label.as_deref().is_some_and(|l| m.ends_content(l)) {
                    flush(&mut current, &mut out);
                    break;
                }
                let heading_in_values = row
                    .label
                    .as_deref()
                    .is_some_and(|l| l.is_empty())
                    .then(|| {
                        let first = row.values.iter().flatten().find(|c| !c.is_empty())?;
                        m.field(first).filter(|(f, _)| f.is_heading())
                    })
                    .flatten();
                if let Some((field, v)) =
                    heading_in_values.or_else(|| row.label.as_deref().and_then(|l| m.field(l)))
                {
                    let continues = current
                        .as_ref()
                        .is_some_and(|o| o.value.field == field && field == Field::Prices);
                    if continues {
                        if let Some(o) = current.as_mut() {
                            o.value.lines.extend(row.cells);
                        }
                        continue;
                    }
                    flush(&mut current, &mut out);
                    current = Some(Open {
                        value: Value {
                            field,
                            lang: v.lang,
                            lines: row.values,
                        },
                        block: Some(row.block),
                        blank: false,
                    });
                    continue;
                }
                if let Some(o) = current.as_mut() {
                    if o.block == Some(row.block) || o.value.field.spans_tables() {
                        let lines = if o.value.field == Field::Prices || row.label.is_none() {
                            row.cells
                        } else {
                            row.values
                        };
                        o.value.lines.extend(lines);
                        o.blank = false;
                    }
                }
            }
            Unit::Plain(text) => {
                if is_index(&text) {
                    flush(&mut current, &mut out);
                    after_index = true;
                    continue;
                }
                let s = strip_plain_index(&text);
                let had_index = s.len() < text.trim().len();
                if m.ends_content(s.split(':').next().unwrap_or(s)) {
                    flush(&mut current, &mut out);
                    break;
                }
                let (label, value) = match s.split_once(':') {
                    Some((l, v)) if m.field(l).is_some() => (l.to_string(), v.trim().to_string()),
                    _ if m
                        .field(s)
                        .is_some_and(|(f, _)| f.stands_alone() || after_index || had_index)
                        && s.chars().count() <= 90 =>
                    {
                        (s.to_string(), String::new())
                    }
                    _ => (String::new(), s.to_string()),
                };
                after_index = false;
                if let Some((field, v)) = m.field(&label) {
                    flush(&mut current, &mut out);
                    current = Some(Open {
                        value: Value {
                            field,
                            lang: v.lang,
                            lines: vec![vec![value]],
                        },
                        block: None,
                        blank: false,
                    });
                    continue;
                }
                if let Some(o) = current.as_mut() {
                    if !o.blank && (o.block.is_none() || o.value.field.spans_tables()) {
                        o.value.lines.push(vec![value]);
                    }
                }
            }
        }
    }
    flush(&mut current, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Forms

#[derive(Default, Debug)]
struct Block {
    instrument: Vec<String>,
    identification: Vec<String>,
    isin: String,
    nature: Option<String>,
    prices: Vec<Vec<String>>,
    aggregated: Vec<Vec<String>>,
    date: Option<String>,
    place: Option<String>,
}

impl Block {
    fn is_empty(&self) -> bool {
        self.instrument.is_empty()
            && self.nature.is_none()
            && self.prices.is_empty()
            && self.date.is_none()
    }
}

#[derive(Default, Debug)]
struct Form {
    name: Option<String>,
    position: Option<String>,
    notification: Option<String>,
    issuer: String,
    lei: String,
    blocks: Vec<Block>,
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    None,
    Person,
    Reason,
    Issuer,
    Transactions,
}

fn isin_in(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .find(|w| {
            w.len() == 12
                && w[..2].chars().all(|c| c.is_ascii_uppercase())
                && w[2..11]
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                && w[11..].chars().all(|c| c.is_ascii_digit())
                && w[2..].chars().any(|c| c.is_ascii_digit())
        })
        .map(str::to_string)
}

fn forms(values: Vec<Value>) -> Vec<Form> {
    let mut forms: Vec<Form> = Vec::new();
    let mut form = Form::default();
    let mut section = Section::None;
    for v in values {
        if v.field == Field::PersonHeading && (form.name.is_some() || !form.blocks.is_empty()) {
            forms.push(std::mem::take(&mut form));
        }
        match v.field {
            Field::PersonHeading => section = Section::Person,
            Field::ReasonHeading => section = Section::Reason,
            Field::IssuerHeading => section = Section::Issuer,
            Field::TransactionHeading => {
                section = Section::Transactions;
                if form.blocks.last().is_some_and(|b| !b.is_empty()) {
                    form.blocks.push(Block::default());
                }
            }
            _ => {}
        }
        if v.field.is_heading() {
            continue;
        }
        let text = v.text();
        match v.field {
            Field::Name if section == Section::Issuer => form.issuer = text,
            Field::Name => {
                if form.name.is_some() && !form.blocks.is_empty() {
                    forms.push(std::mem::take(&mut form));
                }
                form.name.get_or_insert(text);
            }
            Field::Position => {
                form.position.get_or_insert(text);
            }
            Field::Notification => {
                form.notification.get_or_insert(text);
            }
            Field::Lei => {
                if form.lei.is_empty() {
                    form.lei = text.split(' ').next().unwrap_or("").to_string();
                }
            }
            _ => {
                section = Section::Transactions;
                let starts_block = v.field == Field::Instrument
                    && form
                        .blocks
                        .last()
                        .is_some_and(|b| !b.instrument.is_empty() || b.nature.is_some());
                if form.blocks.is_empty() || starts_block {
                    form.blocks.push(Block::default());
                }
                let block = form.blocks.last_mut().expect("a block was just ensured");
                match v.field {
                    Field::Instrument | Field::Identification => {
                        if block.isin.is_empty() {
                            if let Some(isin) = isin_in(&text) {
                                block.isin = isin;
                            }
                        }
                        let first = v.first_line();
                        if !first.is_empty() {
                            if v.field == Field::Instrument {
                                block.instrument.push(first);
                            } else {
                                block.identification.push(first);
                            }
                        }
                    }
                    Field::Nature => block.nature = Some(text),
                    Field::Prices => block.prices.extend(v.lines),
                    Field::Aggregated => block.aggregated.extend(v.lines),
                    Field::Date => block.date = Some(text),
                    Field::Place => block.place = Some(text),
                    _ => {}
                }
            }
        }
    }
    if form.name.is_some() || !form.blocks.is_empty() {
        forms.push(form);
    }
    forms
}

// ---------------------------------------------------------------------------
// Numbers, prices, dates

/// Parse a number as written in a form whose language uses `decimal_comma`.
/// When both separators appear the last one is the decimal separator; a
/// repeated separator groups thousands; a single separator followed by exactly
/// three digits follows the language (`5.000` is 5000 in Danish, `5,000` in
/// English); any other single separator is the decimal separator.
pub fn parse_number(raw: &str, decimal_comma: bool) -> Option<Decimal> {
    let s: String = raw.chars().filter(|c| !is_space(*c)).collect();
    if s.is_empty()
        || !s
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
        || !s.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return None;
    }
    let (dots, commas) = (s.matches('.').count(), s.matches(',').count());
    let normalised = match (dots, commas) {
        (0, 0) => s,
        (d, c) if d > 0 && c > 0 => {
            if s.rfind('.') > s.rfind(',') {
                s.replace(',', "")
            } else {
                s.replace('.', "").replace(',', ".")
            }
        }
        (d, 0) if d > 1 => s.replace('.', ""),
        (0, c) if c > 1 => s.replace(',', ""),
        (_, c) => {
            let sep = if c == 1 { ',' } else { '.' };
            let after = s.len() - s.rfind(sep).unwrap_or(0) - 1;
            let thousands = after == 3 && (sep == ',') != decimal_comma;
            if thousands {
                s.replace(sep, "")
            } else {
                s.replace(sep, ".")
            }
        }
    };
    Decimal::from_str(&normalised).ok()
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Currency(String),
    Number(String),
    Word,
}

fn tokens(cell: &str) -> Vec<Token> {
    cell.split(|c: char| is_space(c) || c == '/')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let t = w.trim_matches(|c: char| matches!(c, '(' | ')' | ':' | ';' | '*'));
            let upper = t.trim_end_matches('.').to_uppercase();
            if CURRENCIES.contains(&upper.as_str()) {
                return Token::Currency(upper);
            }
            let n = t.trim_end_matches(['.', ',']);
            if !n.is_empty()
                && n.chars().next().is_some_and(|c| c.is_ascii_digit())
                && n.chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
            {
                Token::Number(n.to_string())
            } else {
                Token::Word
            }
        })
        .collect()
}

/// A number with the currency next to it in the same cell, if any.
struct Amount {
    raw: String,
    currency: Option<String>,
}

fn amounts(cell: &str) -> Vec<Amount> {
    let toks = tokens(cell);
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if let Token::Number(raw) = t {
            let near = |j: Option<usize>| match j.and_then(|j| toks.get(j)) {
                Some(Token::Currency(c)) => Some(c.clone()),
                _ => None,
            };
            let currency = near(i.checked_sub(1)).or_else(|| near(Some(i + 1)));
            out.push(Amount {
                raw: raw.clone(),
                currency,
            });
        }
    }
    out
}

/// One price/volume pair as read.
struct Pair {
    price: Decimal,
    currency: String,
    volume: Decimal,
}

/// Joins a cell that is only a currency code with the cell below it in the same
/// column (`DKK` / `349.03`, a price wrapped in a narrow column).
fn join_wrapped_currency(lines: &[Vec<String>]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = lines.to_vec();
    for i in 0..out.len().saturating_sub(1) {
        for c in 0..out[i].len() {
            let lone = matches!(tokens(&out[i][c]).as_slice(), [Token::Currency(_)]);
            let below = out[i + 1]
                .get(c)
                .is_some_and(|b| matches!(tokens(b).first(), Some(Token::Number(_))));
            if lone && below {
                let joined = format!("{} {}", out[i][c], out[i + 1][c]);
                out[i + 1][c] = joined;
                out[i][c] = String::new();
            }
        }
    }
    for i in 0..out.len().saturating_sub(1) {
        for c in 0..out[i].len() {
            if is_number_fragment(&out[i], &out[i + 1], c) {
                let joined = format!("{}{}", out[i][c], out[i + 1][c]);
                out[i + 1][c] = joined;
                out[i][c] = String::new();
            }
        }
    }
    out
}

/// True if the cell in column `c` of `below` continues the number that ends the
/// same cell of `above` (a number wrapped in a narrow column: `1,61` / `5.00`,
/// `403` / `.40`): `below` holds only that fragment, and either it starts with
/// a separator or the number above is a short fragment.
fn is_number_fragment(above: &[String], below: &[String], c: usize) -> bool {
    let Some(next) = below.get(c).map(|s| s.trim()) else {
        return false;
    };
    let alone = below
        .iter()
        .enumerate()
        .all(|(k, cell)| k == c || cell.trim().is_empty());
    let numeric = |t: &str| {
        !t.is_empty()
            && t.chars()
                .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == ',')
    };
    let last = above[c].split_whitespace().last().unwrap_or("");
    alone
        && numeric(next)
        && numeric(last)
        && (next.starts_with(['.', ',']) || last.chars().count() <= 4 && !above[c].is_empty())
        && (next.starts_with(['.', ',']) || last.ends_with(|ch: char| ch.is_ascii_digit()))
        && {
            let rest = above
                .iter()
                .enumerate()
                .any(|(k, cell)| k != c && !cell.trim().is_empty());
            next.starts_with(['.', ',']) || !rest || last.contains([',', '.'])
        }
}

fn currency_in(lines: &[Vec<String>]) -> Option<String> {
    lines
        .iter()
        .flatten()
        .flat_map(|c| tokens(c))
        .find_map(|t| match t {
            Token::Currency(c) => Some(c),
            _ => None,
        })
}

/// The price/volume pairs of a block. A price without a currency next to it
/// takes the currency named elsewhere in the field, else the one of the
/// block's aggregated information (`fallback_currency`).
fn pairs(
    lines: &[Vec<String>],
    vocab: &Vocabulary,
    fallback_currency: Option<String>,
) -> Result<Vec<Pair>, MarParseError> {
    let lines = &join_wrapped_currency(lines);
    let default_currency = currency_in(lines).or(fallback_currency);
    let mut out = Vec::new();
    let mut price: Option<(Decimal, String)> = None;
    let mut volume: Option<Decimal> = None;
    for line in lines {
        let lower = collapse(&line.join(" ").to_lowercase());
        if lower.ends_with(':') || vocab.price_summary.iter().any(|w| lower.contains(w)) {
            continue;
        }
        let in_line: Vec<Amount> = line.iter().flat_map(|c| amounts(c)).collect();
        let no_currency = in_line.iter().all(|a| a.currency.is_none());
        for (k, a) in in_line.iter().enumerate() {
            let value = parse_number(&a.raw, vocab.decimal_comma).ok_or_else(|| {
                MarParseError::InvalidNumber {
                    label: "Price(s) and volume(s)",
                    value: a.raw.clone(),
                }
            })?;
            let is_price = a.currency.is_some()
                || (default_currency.is_some()
                    && ((no_currency && in_line.len() == 2 && k == 0)
                        || (in_line.len() == 1 && volume.is_some() && price.is_none())));
            if is_price {
                let currency = a
                    .currency
                    .clone()
                    .or_else(|| default_currency.clone())
                    .unwrap_or_default();
                price = Some((value, currency));
            } else {
                volume = Some(value);
            }
            if let (Some((p, c)), Some(v)) = (price.clone(), volume) {
                if !v.is_zero() {
                    out.push(Pair {
                        price: p,
                        currency: c,
                        volume: v,
                    });
                }
                price = None;
                volume = None;
            }
        }
    }
    Ok(out)
}

/// The aggregated volume of a block, when stated: the first number in the
/// field that is not next to a currency, read in the form's convention and in
/// the other one (an aggregate is sometimes written the other way round).
fn aggregated_volume(lines: &[Vec<String>], vocab: &Vocabulary) -> Vec<Decimal> {
    lines
        .iter()
        .flatten()
        .flat_map(|c| amounts(c))
        .find(|a| a.currency.is_none())
        .map(|a| {
            [vocab.decimal_comma, !vocab.decimal_comma]
                .iter()
                .filter_map(|dc| parse_number(&a.raw, *dc))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_date(raw: &str, langs: &[Lang]) -> Result<NaiveDate, MarParseError> {
    let raw = collapse(raw);
    let raw = match raw.split_once(' ') {
        Some((head, rest)) if head.starts_with('(') && head.ends_with(')') && head.len() <= 4 => {
            rest.to_string()
        }
        _ => raw,
    };
    if let Some(d) = raw
        .get(..10)
        .and_then(|h| NaiveDate::parse_from_str(h, "%Y-%m-%d").ok())
    {
        return Ok(d);
    }
    if let Some(d) = ["%d.%m.%Y", "%d/%m/%Y", "%d-%m-%Y"]
        .iter()
        .find_map(|f| NaiveDate::parse_from_str(&raw, f).ok())
    {
        return Ok(d);
    }
    let words: Vec<String> = raw
        .split(|c: char| c.is_whitespace() || c == ',' || c == '.')
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    if words.len() == 3 {
        let month_of = |w: &str| {
            langs
                .iter()
                .find_map(|l| report_language(*l).month_number(w, None))
        };
        let (day, month, year) = match (month_of(&words[0]), month_of(&words[1])) {
            (None, Some(m)) => (&words[0], m, &words[2]),
            (Some(m), None) => (&words[1], m, &words[2]),
            _ => return Err(MarParseError::InvalidDate(raw)),
        };
        if let (Ok(d), Ok(y)) = (day.parse::<u32>(), year.parse::<i32>()) {
            if let Some(date) = NaiveDate::from_ymd_opt(y, u32::from(month), d) {
                return Ok(date);
            }
        }
    }
    Err(MarParseError::InvalidDate(raw))
}

/// The transaction type of a nature text. A nature that starts with a sale
/// word is a sale whatever follows (`Sale of shares (to cover tax on shares
/// vested under ...)`); otherwise gifts, exercises, grants and other transfers
/// are recognised anywhere in the text, and only a nature that starts with an
/// acquisition word is a buy.
fn classify(nature: &str, vocab: &Vocabulary) -> UnifiedTransactionType {
    use UnifiedTransactionType as T;
    let n = collapse(&nature.to_lowercase());
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    let starts = |words: &[&str]| {
        words
            .iter()
            .any(|w| n == *w || n.starts_with(&format!("{w} ")))
    };
    let w = &vocab.natures;
    if starts(w.sell) {
        T::Sell
    } else if has(w.gift) {
        T::Gift
    } else if has(w.exercise) {
        T::OptionExercise
    } else if has(w.grant) {
        T::Grant
    } else if has(w.other) {
        T::Other
    } else if starts(w.buy) {
        T::Buy
    } else {
        T::Other
    }
}

// ---------------------------------------------------------------------------
// Entry point

/// Parse the text of one release into its transactions. `langs` are the
/// languages of the issuer's country; the form's own language (the one whose
/// labels matched most) decides how numbers are read and is returned as each
/// transaction's `lang`. `reference_number` is empty: the template has none.
pub fn parse(text: &str, langs: &[Lang]) -> Result<Vec<ParsedMarTransaction>, MarParseError> {
    let vocabs = vocabularies(langs);
    let matcher = Matcher { vocabs: &vocabs };
    let values = fields(text, &matcher);
    let form_fields = values
        .iter()
        .filter(|v| matches!(v.field, Field::Prices | Field::PersonHeading))
        .count();
    if form_fields == 0 {
        let lower = text.to_lowercase();
        if vocabs
            .iter()
            .flat_map(|v| v.short_form.iter())
            .any(|w| lower.contains(w))
            && values.iter().any(|v| v.field == Field::Name)
        {
            return Err(MarParseError::UnsupportedForm);
        }
        return Err(MarParseError::FormNotInText);
    }
    let lang = form_language(&values, &vocabs);
    let vocab = vocabulary(lang).expect("the form language has a vocabulary");

    let mut out = Vec::new();
    for form in forms(values) {
        let name = form
            .name
            .filter(|n| !n.is_empty())
            .ok_or(MarParseError::MissingName)?;
        let position = form
            .position
            .filter(|p| !p.is_empty())
            .ok_or(MarParseError::MissingPosition)?;
        let notification = form
            .notification
            .ok_or(MarParseError::MissingNotificationType)?;
        let note = notification.to_lowercase();
        let is_amendment = if vocabs
            .iter()
            .flat_map(|v| v.amendment.iter())
            .any(|w| note.contains(w))
        {
            true
        } else if vocabs
            .iter()
            .flat_map(|v| v.initial.iter())
            .any(|w| note.contains(w))
        {
            false
        } else {
            return Err(MarParseError::MissingNotificationType);
        };
        let pos_lower = position.to_lowercase();
        let is_close_associate = vocabs
            .iter()
            .flat_map(|v| v.close_associate.iter())
            .any(|w| pos_lower.contains(w));
        if form.blocks.is_empty() {
            return Err(MarParseError::MissingTransactionDate);
        }
        for block in form.blocks {
            let instrument = block
                .instrument
                .iter()
                .chain(block.identification.iter())
                .map(|i| strip_isin(i, &block.isin))
                .find(|i| !i.is_empty())
                .ok_or(MarParseError::MissingInstrument)?;
            let nature = block
                .nature
                .filter(|n| !n.is_empty())
                .ok_or(MarParseError::MissingNature)?;
            let date_raw = block.date.ok_or(MarParseError::MissingTransactionDate)?;
            let transaction_date = parse_date(&date_raw, langs)?;
            let transaction_type = classify(&nature, vocab);
            let found = pairs(&block.prices, vocab, currency_in(&block.aggregated))?;
            if found.is_empty() {
                let lower = block
                    .prices
                    .iter()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                if vocabs
                    .iter()
                    .flat_map(|v| v.see_attachment.iter())
                    .any(|w| lower.contains(w))
                {
                    return Err(MarParseError::FormNotInText);
                }
                return Err(MarParseError::MissingVolume);
            }
            let stated = aggregated_volume(&block.aggregated, vocab);
            let sum: Decimal = found.iter().map(|p| p.volume).sum();
            if !stated.is_empty() && !stated.contains(&sum) {
                return Err(MarParseError::AggregateMismatch {
                    stated: stated[0].to_string(),
                    sum: sum.to_string(),
                });
            }
            for pair in found {
                let index = out.len();
                if pair.currency.is_empty() {
                    return Err(MarParseError::InvalidCurrency(pair.price.to_string()));
                }
                if pair.price.is_zero()
                    && matches!(
                        transaction_type,
                        UnifiedTransactionType::Buy | UnifiedTransactionType::Sell
                    )
                {
                    return Err(MarParseError::SuspiciousZeroPrice {
                        index,
                        nature: nature.clone(),
                    });
                }
                out.push(ParsedMarTransaction {
                    index,
                    person_name: normalise_name(&name),
                    person_role: position.clone(),
                    is_close_associate,
                    issuer_name: form.issuer.clone(),
                    lei: form.lei.clone(),
                    reference_number: String::new(),
                    is_amendment,
                    amendment_reason: if is_amendment {
                        notification.clone()
                    } else {
                        String::new()
                    },
                    original_reference_number: None,
                    transaction_date,
                    venue: block.place.clone().filter(|p| !p.is_empty()),
                    instrument_type: instrument.clone(),
                    isin: block.isin.clone(),
                    nature: nature.clone(),
                    transaction_type,
                    volume: pair.volume,
                    unit_price: Some(pair.price),
                    currency: pair.currency,
                    lang: lang.code().to_string(),
                });
            }
        }
    }
    if out.is_empty() {
        return Err(MarParseError::MissingVolume);
    }
    check_prices(&out)?;
    Ok(out)
}

/// Refuses a release whose non-zero prices of one currency span more than a
/// factor of 100: a thousands separator read as a decimal one (or the other way
/// round) moves a price by 1000.
fn check_prices(out: &[ParsedMarTransaction]) -> Result<(), MarParseError> {
    let hundred = Decimal::from(100);
    for t in out {
        let Some(p) = t.unit_price.filter(|p| !p.is_zero()) else {
            continue;
        };
        let off = out.iter().any(|o| {
            o.currency == t.currency
                && o.unit_price
                    .filter(|q| !q.is_zero())
                    .is_some_and(|q| p > q * hundred)
        });
        if off {
            return Err(MarParseError::InvalidNumber {
                label: "Price(s)",
                value: p.to_string(),
            });
        }
    }
    Ok(())
}

/// The language whose labels matched most; the first language on a tie.
fn form_language(values: &[Value], vocabs: &[&'static Vocabulary]) -> Lang {
    vocabs
        .iter()
        .map(|v| (values.iter().filter(|x| x.lang == v.lang).count(), v.lang))
        .fold(None::<(usize, Lang)>, |best, (n, l)| match best {
            Some((b, _)) if b >= n => best,
            _ => Some((n, l)),
        })
        .map(|(_, l)| l)
        .unwrap_or(Lang::En)
}

/// `Shares (ISIN: GB00BZ3CNK81)` -> `Shares`; `Novo Nordisk B DK0062498333` ->
/// `Novo Nordisk B`.
fn strip_isin(instrument: &str, isin: &str) -> String {
    let mut s = instrument.to_string();
    if !isin.is_empty() {
        s = s.replace(isin, "");
    }
    let s = s.replace("ISIN:", "").replace("ISIN", "").replace("()", "");
    collapse(s.trim_matches(|c: char| c == '(' || c == ')' || c == ',' || c.is_whitespace()))
}
