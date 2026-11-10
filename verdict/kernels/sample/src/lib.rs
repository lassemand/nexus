//! Sample kernel for generic interim reports.
//!
//! It reads EBIT margin, net sales, EBIT, net income and earnings per share in
//! English, Swedish, Danish and Finnish, and decides with a placeholder rule:
//! net sales growth over 5 % and EBIT up is `buy`, both down is `no_buy`,
//! anything else (or prior net sales of zero) is `abstain`.
//!
//! The lookup is this kernel's own code, in five variants that give the same
//! verdict and differ only in how they find the figures:
//!
//! * [`analyze_anchored`] (a): known lines and `starts_with`, a direct number
//!   read, and a fallback to (c) when a check fails. This is what
//!   [`analyze`] runs.
//! * [`analyze_memmem`] (b): one `memmem` per label inside the first 2 KB.
//! * [`analyze_aho`] (c): one Aho-Corasick pass.
//! * [`analyze_regex`] (d): one regex per metric.
//! * [`analyze_lowercase`] (e): `to_lowercase` + `find`.
//!
//! It exists for tests, the analyst runtime benchmark and as the worked example
//! in `verdict/AGENT_GUIDE.md`. A real kernel's rule is company-specific.

mod labels;
mod number;

use std::sync::LazyLock;

use memchr::memmem;
use regex::Regex;
use verdict_sdk::{export_kernel, Decimal, Decision, Lang, Metric, Report, Verdict};

use labels::{left_boundary, right_boundary, Labels};
pub use number::{current_and_prior, next_number, style_of, unit_after, Num, NumberStyle};

/// Metric keys, in the order of [`PHRASES`].
pub const KEYS: [&str; 5] = ["ebit_margin", "net_sales", "ebit", "net_income", "eps"];

const NET_SALES: usize = 1;
const EBIT: usize = 2;

/// Label phrases per metric (en, sv, da, fi). A trailing `*` is an open stem.
pub const PHRASES: [&[&str]; 5] = [
    &[
        "ebit margin",
        "ebit-margin",
        "operating margin",
        "rörelsemarginal*",
        "ebit-marginal*",
        "overskudsgrad*",
        "ebit-marginaali",
        "liikevoittoprosentti",
        "liiketulosprosentti",
    ],
    &[
        "net sales",
        "revenue*",
        "net revenue*",
        "nettoomsättning*",
        "omsättning*",
        "nettoomsætning*",
        "omsætning*",
        "liikevaihto*",
    ],
    &[
        "ebit",
        "operating profit",
        "operating income",
        "rörelseresultat*",
        "resultat af primær drift",
        "primært resultat",
        "liikevoitto*",
        "liiketulos*",
    ],
    &[
        "net income",
        "net profit",
        "profit for the period",
        "periodens resultat",
        "periodens resultat efter skatt",
        "nettoresultat*",
        "årets resultat",
        "kauden tulos",
        "katsauskauden tulos",
    ],
    &[
        "earnings per share",
        "eps",
        "resultat per aktie",
        "periodens resultat per aktie",
        "årets resultat per aktie",
        "osakekohtainen tulos",
    ],
];

const WINDOW: usize = 200;
const MAX_FIGURES: usize = 4;
const MEMMEM_WINDOW: usize = 2048;
const MIN_GROWTH_PCT: Decimal = Decimal::from_parts(5, 0, 0, false, 0);
const HUNDRED: Decimal = Decimal::ONE_HUNDRED;
const MARGIN: usize = 0;

static LABELS: LazyLock<Labels> = LazyLock::new(|| Labels::new(&PHRASES));

/// Exact-casing phrases for variant (b), as this company writes them.
const EXACT: [&[&str]; 5] = [
    &["EBIT margin", "Rörelsemarginal"],
    &["Net sales", "Nettoomsättning"],
    &["Operating profit", "Rörelseresultat"],
    &["Profit for the period", "Periodens resultat uppgick"],
    &["Earnings per share", "Periodens resultat per aktie"],
];

static FINDERS: LazyLock<Vec<Vec<memmem::Finder<'static>>>> = LazyLock::new(|| {
    EXACT
        .iter()
        .map(|phrases| phrases.iter().map(memmem::Finder::new).collect())
        .collect()
});

static REGEXES: LazyLock<Vec<Option<Regex>>> = LazyLock::new(|| {
    PHRASES
        .iter()
        .map(|phrases| {
            let alternatives: Vec<String> = phrases
                .iter()
                .map(|phrase| match phrase.strip_suffix('*') {
                    Some(stem) => format!(r"{}\w*", regex::escape(stem)),
                    None => format!(r"{}\b", regex::escape(phrase)),
                })
                .collect();
            Regex::new(&format!(r"(?i)\b(?:{})", alternatives.join("|"))).ok()
        })
        .collect()
});

static LOWERED: LazyLock<Vec<Vec<(String, bool)>>> = LazyLock::new(|| {
    PHRASES
        .iter()
        .map(|phrases| {
            phrases
                .iter()
                .map(|phrase| match phrase.strip_suffix('*') {
                    Some(stem) => (stem.to_lowercase(), true),
                    None => (phrase.to_lowercase(), false),
                })
                .collect()
        })
        .collect()
});

/// One anchored layout: the heading line, then one line per metric.
struct Layout {
    heading: &'static str,
    lines: [(usize, &'static str); 5],
}

const LAYOUTS: [(Lang, Layout); 2] = [
    (
        Lang::En,
        Layout {
            heading: "Third quarter",
            lines: [
                (1, "• Net sales"),
                (2, "• Operating profit"),
                (MARGIN, "• EBIT margin"),
                (3, "• Profit for the period"),
                (4, "• Earnings per share"),
            ],
        },
    ),
    (
        Lang::Sv,
        Layout {
            heading: "Tredje kvartalet",
            lines: [
                (1, "• Nettoomsättningen"),
                (2, "• Rörelseresultatet"),
                (MARGIN, "• Rörelsemarginalen"),
                (3, "• Periodens resultat uppgick"),
                (4, "• Periodens resultat per aktie"),
            ],
        },
    ),
];

#[derive(Clone, Copy)]
struct Reading<'a> {
    current: Num,
    prior: Option<Num>,
    unit: Option<&'a str>,
}

type Readings<'a> = [Option<Reading<'a>>; 5];

fn is_percent(unit: Option<&str>) -> bool {
    unit.is_some_and(|unit| {
        unit == "%"
            || unit.eq_ignore_ascii_case("procent")
            || unit.eq_ignore_ascii_case("prosent")
            || unit.eq_ignore_ascii_case("prosenttia")
    })
}

fn read(
    text: &str,
    from: usize,
    window: usize,
    style: NumberStyle,
    skip_percent: bool,
) -> Option<Reading<'_>> {
    let end = from.saturating_add(window);
    let mut at = from;
    for _ in 0..MAX_FIGURES {
        let (current, prior) = current_and_prior(text, at, end.saturating_sub(at), style)?;
        let unit = unit_after(text, current.end);
        if !(skip_percent && is_percent(unit)) {
            return Some(Reading {
                current,
                prior,
                unit,
            });
        }
        at = current.end;
    }
    None
}

fn read_label(
    text: &str,
    hit_end: usize,
    style: NumberStyle,
    metric: usize,
) -> Option<Reading<'_>> {
    read(text, hit_end, WINDOW, style, metric != MARGIN)
}

fn shadowed(text: &str, start: usize, len: usize, metric: usize) -> bool {
    let Some(rest) = text.as_bytes().get(start..) else {
        return false;
    };
    PHRASES.iter().enumerate().any(|(other, phrases)| {
        other != metric
            && phrases.iter().any(|phrase| {
                let stem = phrase.strip_suffix('*').unwrap_or(phrase).as_bytes();
                stem.len() > len
                    && rest
                        .get(..stem.len())
                        .is_some_and(|head| head.eq_ignore_ascii_case(stem))
            })
    })
}

fn next_line(text: &str, at: usize) -> Option<(&str, usize)> {
    let rest = text.get(at..)?;
    if rest.is_empty() {
        return None;
    }
    let len = memchr::memchr(b'\n', rest.as_bytes()).unwrap_or(rest.len());
    let line = rest.get(..len)?.trim_end_matches('\r');
    Some((line, at + len + 1))
}

/// True when the anchored lookup (a) verifies on this release: the heading and
/// every prefix match, each figure parses, and the units are right. When it is
/// false [`analyze_anchored`] falls back to (c).
pub fn anchored_applies(report: &Report) -> bool {
    anchored_readings(report).is_some()
}

fn anchored_readings<'a>(report: &Report<'a>) -> Option<Readings<'a>> {
    let text = report.text;
    let layout = &LAYOUTS.iter().find(|(lang, _)| *lang == report.lang)?.1;
    let style = style_of(report.lang);
    let (_, at) = next_line(text, 0)?;
    let (_, at) = next_line(text, at)?;
    let (heading, mut at) = next_line(text, at)?;
    if !heading.starts_with(layout.heading) {
        return None;
    }
    let mut readings: Readings = [None; 5];
    for (metric, prefix) in layout.lines {
        let (line, next) = next_line(text, at)?;
        let rest = line.strip_prefix(prefix)?;
        let from = text.len().checked_sub(text.get(at..)?.len())? + (line.len() - rest.len());
        let reading = read(text, from, rest.len(), style, metric != MARGIN)?;
        if is_percent(reading.unit) != (metric == MARGIN) {
            return None;
        }
        *readings.get_mut(metric)? = Some(reading);
        at = next;
    }
    Some(readings)
}

fn memmem_readings<'a>(report: &Report<'a>) -> Readings<'a> {
    let text = report.text;
    let style = style_of(report.lang);
    let window = text
        .as_bytes()
        .get(..MEMMEM_WINDOW)
        .unwrap_or(text.as_bytes());
    let mut readings: Readings = [None; 5];
    for (metric, finders) in FINDERS.iter().enumerate() {
        let earliest = finders
            .iter()
            .zip(EXACT[metric])
            .filter_map(|(finder, phrase)| {
                finder
                    .find_iter(window)
                    .find(|start| left_boundary(text, *start))
                    .map(|start| start + phrase.len())
            })
            .min();
        if let (Some(end), Some(slot)) = (earliest, readings.get_mut(metric)) {
            *slot = read_label(text, end, style, metric);
        }
    }
    readings
}

fn aho_readings<'a>(report: &Report<'a>) -> Readings<'a> {
    let text = report.text;
    let style = style_of(report.lang);
    let mut readings: Readings = [None; 5];
    for (metric, hit) in LABELS.first_hits(text).into_iter().enumerate() {
        if let (Some(hit), Some(slot)) = (hit, readings.get_mut(metric)) {
            *slot = read_label(text, hit.end, style, metric);
        }
    }
    readings
}

fn regex_readings<'a>(report: &Report<'a>) -> Readings<'a> {
    let text = report.text;
    let style = style_of(report.lang);
    let mut readings: Readings = [None; 5];
    for (metric, regex) in REGEXES.iter().enumerate() {
        let hit = regex.as_ref().and_then(|regex| {
            regex
                .find_iter(text)
                .find(|found| !shadowed(text, found.start(), found.len(), metric))
        });
        if let (Some(hit), Some(slot)) = (hit, readings.get_mut(metric)) {
            *slot = read_label(text, hit.end(), style, metric);
        }
    }
    readings
}

fn lowercase_readings<'a>(report: &Report<'a>) -> Readings<'a> {
    let text = report.text;
    let style = style_of(report.lang);
    let lowered = text.to_lowercase();
    let mut readings: Readings = [None; 5];
    if lowered.len() != text.len() {
        return readings;
    }
    for (metric, phrases) in LOWERED.iter().enumerate() {
        let earliest = phrases
            .iter()
            .filter_map(|(phrase, open)| {
                lowered
                    .match_indices(phrase.as_str())
                    .find(|(start, _)| {
                        let end = start + phrase.len();
                        left_boundary(&lowered, *start)
                            && (*open || right_boundary(&lowered, end))
                            && !shadowed(&lowered, *start, phrase.len(), metric)
                    })
                    .map(|(start, _)| start + phrase.len())
            })
            .min();
        if let (Some(end), Some(slot)) = (earliest, readings.get_mut(metric)) {
            *slot = read_label(text, end, style, metric);
        }
    }
    readings
}

fn decide(sales: Option<&Reading>, ebit: Option<&Reading>) -> (Decision, String) {
    let abstain = |reason: &str| (Decision::Abstain, reason.to_owned());
    let Some(sales) = sales else {
        return abstain("net sales not found");
    };
    let Some(sales_prior) = sales.prior else {
        return abstain("prior net sales not found");
    };
    let Some(ebit) = ebit else {
        return abstain("EBIT not found");
    };
    let Some(ebit_prior) = ebit.prior else {
        return abstain("prior EBIT not found");
    };
    if sales_prior.value.is_zero() {
        return abstain("prior net sales is zero");
    }
    let growth = (sales.current.value - sales_prior.value)
        .checked_mul(HUNDRED)
        .and_then(|change| change.checked_div(sales_prior.value.abs()));
    let Some(growth) = growth else {
        return abstain("net sales growth overflowed");
    };
    let growth = growth.round_dp(1);
    let ebit_up = ebit.current.value > ebit_prior.value;
    let ebit_down = ebit.current.value < ebit_prior.value;
    if growth > MIN_GROWTH_PCT && ebit_up {
        (
            Decision::Buy,
            format!("net sales up {growth} % and EBIT up"),
        )
    } else if growth < Decimal::ZERO && ebit_down {
        (
            Decision::NoBuy,
            format!("net sales down {} % and EBIT down", growth.abs()),
        )
    } else {
        (
            Decision::Abstain,
            format!("mixed result: net sales growth {growth} %, EBIT up {ebit_up}"),
        )
    }
}

fn verdict(readings: &Readings) -> Verdict {
    let (decision, reason) = decide(
        readings.get(NET_SALES).and_then(Option::as_ref),
        readings.get(EBIT).and_then(Option::as_ref),
    );
    let metrics = KEYS
        .iter()
        .zip(readings)
        .filter_map(|(key, reading)| {
            reading.as_ref().map(|reading| Metric {
                key,
                current: Some(reading.current.value.to_string()),
                prior: reading.prior.map(|prior| prior.value.to_string()),
                expected: None,
                unit: reading.unit.map(str::to_owned),
            })
        })
        .collect();
    Verdict {
        decision,
        reason,
        metrics,
    }
}

/// Variant (a): the anchored lookup, falling back to one Aho-Corasick pass
/// when a check fails (a changed heading, prefix, number or unit).
pub fn analyze_anchored(report: &Report) -> Verdict {
    match anchored_readings(report) {
        Some(readings) => verdict(&readings),
        None => verdict(&aho_readings(report)),
    }
}

/// Variant (b): one `memmem` per label inside the first 2 KB.
pub fn analyze_memmem(report: &Report) -> Verdict {
    verdict(&memmem_readings(report))
}

/// Variant (c): one Aho-Corasick pass over all labels.
pub fn analyze_aho(report: &Report) -> Verdict {
    verdict(&aho_readings(report))
}

/// Variant (d): one regex per metric, compiled once.
pub fn analyze_regex(report: &Report) -> Verdict {
    verdict(&regex_readings(report))
}

/// Variant (e): `to_lowercase` + `find`. Abstains for lack of metrics when
/// lowercasing changes the text's length, because the offsets would be wrong.
pub fn analyze_lowercase(report: &Report) -> Verdict {
    verdict(&lowercase_readings(report))
}

/// Reads the five metrics and applies the placeholder decision rule, with the
/// anchored lookup of [`analyze_anchored`].
pub fn analyze(report: &Report) -> Verdict {
    analyze_anchored(report)
}

export_kernel!(crate::analyze);
