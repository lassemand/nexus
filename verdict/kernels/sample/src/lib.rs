//! Worked example: a kernel for one fictional company's interim reports.
//!
//! It shows *one* way to read one report layout, in English and Swedish, and
//! decides with a placeholder rule: net sales growth over 5 % and EBIT up is
//! `buy`, both down is `no_buy`, anything else (or prior net sales of zero) is
//! `abstain`.
//!
//! Nothing here is standard. Which metrics matter, what to look for (a
//! heading, a line prefix, a phrase, a value) and how to look for it are the
//! kernel author's decisions for that company, taken from its previous reports.
//! The five lookups in [`variants`] each hold their own labels for this one
//! company; they give the same verdict and differ only in speed. They exist for
//! tests, the analyst runtime benchmark and as the worked example in
//! `verdict/AGENT_GUIDE.md`. [`number`] is one company's number format.

pub mod number;
pub mod variants;

use verdict_sdk::{export_kernel, Decimal, Decision, Metric, Report, Verdict};

use number::{current_and_prior, style_of, unit_after, Num, NumberStyle};
use variants::Ends;

/// Metric keys, in the order of the lookups' [`Ends`].
pub const KEYS: [&str; 2] = ["net_sales", "ebit"];

const FIGURE_WINDOW: usize = 200;
const MAX_FIGURES: usize = 4;
const MIN_GROWTH_PCT: Decimal = Decimal::from_parts(5, 0, 0, false, 0);

#[derive(Clone, Copy)]
struct Reading<'a> {
    current: Num,
    prior: Option<Num>,
    unit: Option<&'a str>,
}

fn is_percent(unit: Option<&str>) -> bool {
    unit.is_some_and(|unit| unit == "%" || unit.eq_ignore_ascii_case("procent"))
}

fn read(text: &str, from: usize, style: NumberStyle) -> Option<Reading<'_>> {
    let end = from.saturating_add(FIGURE_WINDOW);
    let mut at = from;
    for _ in 0..MAX_FIGURES {
        let (current, prior) = current_and_prior(text, at, end.saturating_sub(at), style)?;
        let unit = unit_after(text, current.end);
        if !is_percent(unit) {
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

fn readings<'a>(text: &'a str, style: NumberStyle, ends: &Ends) -> [Option<Reading<'a>>; 2] {
    ends.map(|end| end.and_then(|end| read(text, end, style)))
}

fn verified(readings: &[Option<Reading>; 2]) -> bool {
    readings
        .iter()
        .all(|reading| reading.is_some_and(|reading| reading.prior.is_some()))
}

fn decide(sales: Option<Reading>, ebit: Option<Reading>) -> (Decision, String) {
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
        .checked_mul(Decimal::ONE_HUNDRED)
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

fn verdict(readings: &[Option<Reading>; 2]) -> Verdict {
    let [sales, ebit] = *readings;
    let (decision, reason) = decide(sales, ebit);
    let metrics = KEYS
        .iter()
        .zip(readings)
        .filter_map(|(key, reading)| {
            reading.map(|reading| Metric {
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

/// Variant (a), and the kernel that is exported: the anchored lookup, verified
/// (the heading and prefixes match, both figures parse with a prior figure),
/// falling back to one Aho-Corasick pass (c) when a check fails. If that finds
/// nothing either, the verdict abstains with the reason.
pub fn analyze_anchored(report: &Report) -> Verdict {
    let text = report.text;
    let style = style_of(report.lang);
    if let Some(ends) = variants::anchored(text, report.lang) {
        let found = readings(text, style, &ends);
        if verified(&found) {
            return verdict(&found);
        }
    }
    verdict(&readings(text, style, &variants::automaton(text)))
}

/// Variant (b): one `memmem` per label inside the first 2 KB.
pub fn analyze_window(report: &Report) -> Verdict {
    let style = style_of(report.lang);
    verdict(&readings(
        report.text,
        style,
        &variants::window(report.text, report.lang),
    ))
}

/// Variant (c): one Aho-Corasick pass.
pub fn analyze_automaton(report: &Report) -> Verdict {
    let style = style_of(report.lang);
    verdict(&readings(
        report.text,
        style,
        &variants::automaton(report.text),
    ))
}

/// Variant (d): one regex per metric.
pub fn analyze_pattern(report: &Report) -> Verdict {
    let style = style_of(report.lang);
    verdict(&readings(
        report.text,
        style,
        &variants::pattern(report.text),
    ))
}

/// Variant (e): `to_lowercase` + `find`.
pub fn analyze_lowercase(report: &Report) -> Verdict {
    let style = style_of(report.lang);
    verdict(&readings(
        report.text,
        style,
        &variants::lowercase(report.text),
    ))
}

/// The exported kernel: [`analyze_anchored`].
pub fn analyze(report: &Report) -> Verdict {
    analyze_anchored(report)
}

export_kernel!(crate::analyze);
