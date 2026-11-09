//! The sample kernel on English and Swedish fixtures.

use verdict_sample::analyze;
use verdict_sdk::{run_into, Decision, Lang, Metric, Report, Verdict};

const EN: &str = include_str!("fixtures/en_interim.txt");
const SV: &str = include_str!("fixtures/sv_interim.txt");

fn metric(key: &'static str, current: &str, prior: &str, unit: Option<&str>) -> Metric {
    Metric {
        key,
        current: Some(current.to_owned()),
        prior: Some(prior.to_owned()),
        expected: None,
        unit: unit.map(str::to_owned),
    }
}

fn run(text: &str, lang: Lang) -> Verdict {
    analyze(&Report { text, lang })
}

#[test]
fn english_interim() {
    let verdict = run(EN, Lang::En);
    assert_eq!(
        verdict.metrics,
        vec![
            metric("ebit_margin", "11.9", "9.8", Some("%")),
            metric("net_sales", "812.4", "725.1", Some("million")),
            metric("ebit", "96.3", "71.0", Some("million")),
            metric("net_income", "70.2", "52.6", Some("million")),
            metric("eps", "1.84", "1.38", None),
        ]
    );
    assert_eq!(verdict.decision, Decision::Buy);
    assert_eq!(verdict.reason, "net sales up 12.0 % and EBIT up");
}

#[test]
fn swedish_interim() {
    let verdict = run(SV, Lang::Sv);
    assert_eq!(
        verdict.metrics,
        vec![
            metric("ebit_margin", "11.9", "9.8", Some("procent")),
            metric("net_sales", "812.4", "725.1", Some("MSEK")),
            metric("ebit", "96.3", "71.0", Some("MSEK")),
            metric("net_income", "70.2", "52.6", Some("MSEK")),
            metric("eps", "1.84", "1.38", Some("kronor")),
        ]
    );
    assert_eq!(verdict.decision, Decision::Buy);
}

#[test]
fn eps_long_form_beats_net_income() {
    let verdict = run(
        "Periodens resultat per aktie uppgick till 1,84 kronor (1,38)",
        Lang::Sv,
    );
    assert_eq!(
        verdict.metrics,
        vec![metric("eps", "1.84", "1.38", Some("kronor"))]
    );
}

#[test]
fn both_down_is_no_buy() {
    let text = "Net sales amounted to SEK 90.0 million (100.0). \
                Operating profit was SEK -6.1 million (4.0).";
    let verdict = run(text, Lang::En);
    assert_eq!(verdict.decision, Decision::NoBuy);
    assert_eq!(verdict.reason, "net sales down 10 % and EBIT down");
}

#[test]
fn loss_narrowing_counts_as_ebit_up() {
    let text = "Net sales amounted to SEK 120.0 million (100.0). \
                Operating profit was SEK -6.1 million (-11.7).";
    assert_eq!(run(text, Lang::En).decision, Decision::Buy);
}

#[test]
fn mixed_or_missing_abstains() {
    let mixed = "Net sales SEK 120.0 million (100.0). EBIT SEK 3.0 million (4.0).";
    assert_eq!(run(mixed, Lang::En).decision, Decision::Abstain);
    let zero = "Net sales SEK 12.0 million (0). EBIT SEK 3.0 million (-4.0).";
    let verdict = run(zero, Lang::En);
    assert_eq!(verdict.decision, Decision::Abstain);
    assert_eq!(verdict.reason, "prior net sales is zero");
}

#[test]
fn unrelated_release_abstains() {
    let text = "Example Group AB appoints new Chief Financial Officer\n\n\
                The board has appointed a new CFO, who will take up the position on 1 March 2027.";
    let verdict = run(text, Lang::En);
    assert_eq!(verdict.decision, Decision::Abstain);
    assert_eq!(verdict.reason, "net sales not found");
    assert!(verdict.metrics.is_empty());
}

#[test]
fn run_into_matches_direct_call() {
    let mut out = Vec::new();
    run_into(analyze, EN.as_bytes(), Lang::En.code(), &mut out);
    let direct = serde_json::to_vec(&run(EN, Lang::En)).expect("a verdict serializes");
    assert_eq!(out, direct);
}
