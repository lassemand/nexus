//! Sample kernel for generic interim reports.
//!
//! It reads EBIT margin, net sales, EBIT, net income and earnings per share in
//! English, Swedish, Danish and Finnish, and decides with a placeholder rule:
//! net sales growth over 5 % and EBIT up is `buy`, both down is `no_buy`,
//! anything else (or prior net sales of zero) is `abstain`.
//!
//! It exists for tests, the analyst runtime benchmark and as the worked example
//! in `verdict/AGENT_GUIDE.md`. A real kernel's rule is company-specific.

use std::sync::LazyLock;

use verdict_sdk::{
    current_and_prior, export_kernel, unit_after, Decimal, Decision, Labels, Metric, Num,
    NumberStyle, Report, Verdict,
};

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

static LABELS: LazyLock<Labels> = LazyLock::new(|| Labels::new(&PHRASES));

const WINDOW: usize = 200;
const MAX_FIGURES: usize = 4;
const MIN_GROWTH_PCT: Decimal = Decimal::from_parts(5, 0, 0, false, 0);
const HUNDRED: Decimal = Decimal::ONE_HUNDRED;

struct Reading<'a> {
    current: Num,
    prior: Option<Num>,
    unit: Option<&'a str>,
}

fn is_percent(unit: Option<&str>) -> bool {
    unit.is_some_and(|unit| {
        unit == "%"
            || unit.eq_ignore_ascii_case("procent")
            || unit.eq_ignore_ascii_case("prosent")
            || unit.eq_ignore_ascii_case("prosenttia")
    })
}

fn read(text: &str, from: usize, style: NumberStyle, skip_percent: bool) -> Option<Reading<'_>> {
    let end = from.saturating_add(WINDOW);
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

/// Reads the five metrics and applies the placeholder decision rule.
pub fn analyze(report: &Report) -> Verdict {
    let text = report.text;
    let style = report.lang.number_style();
    let readings: Vec<Option<Reading>> = LABELS
        .first_hits(text)
        .into_iter()
        .enumerate()
        .map(|(metric, hit)| hit.and_then(|hit| read(text, hit.end, style, metric != 0)))
        .collect();
    let (decision, reason) = decide(
        readings.get(NET_SALES).and_then(Option::as_ref),
        readings.get(EBIT).and_then(Option::as_ref),
    );
    let metrics = KEYS
        .iter()
        .zip(&readings)
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

export_kernel!(crate::analyze);
