//! The sample kernel on English and Swedish fixtures.

use verdict_sample::{
    analyze, analyze_aho, analyze_anchored, analyze_lowercase, analyze_memmem, analyze_regex,
    anchored_applies,
};
use verdict_sdk::harness::{assert_abstains, check_fixtures, time_per_release, Fixture};
use verdict_sdk::{run_into, Analyze, Decision, Lang, Metric, Report, Verdict};

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

const VARIANTS: [(&str, Analyze); 5] = [
    ("anchored", analyze_anchored),
    ("memmem", analyze_memmem),
    ("aho", analyze_aho),
    ("regex", analyze_regex),
    ("lowercase", analyze_lowercase),
];

fn fixtures() -> Vec<Fixture<'static>> {
    let expected_en = vec![
        ("ebit_margin", Some("11.9"), Some("9.8")),
        ("net_sales", Some("812.4"), Some("725.1")),
        ("ebit", Some("96.3"), Some("71.0")),
        ("net_income", Some("70.2"), Some("52.6")),
        ("eps", Some("1.84"), Some("1.38")),
    ];
    vec![
        Fixture {
            name: "en_interim.txt",
            text: EN,
            lang: Lang::En,
            expected: expected_en.clone(),
            expected_decision: Decision::Buy,
        },
        Fixture {
            name: "sv_interim.txt",
            text: SV,
            lang: Lang::Sv,
            expected: expected_en,
            expected_decision: Decision::Buy,
        },
    ]
}

#[test]
fn every_variant_passes_the_fixtures() {
    for (name, variant) in VARIANTS {
        if let Err(diff) = check_fixtures(variant, &fixtures()) {
            panic!("{name}:\n{diff}");
        }
    }
}

#[test]
fn every_variant_gives_the_same_verdict() {
    for (text, lang) in [(EN, Lang::En), (SV, Lang::Sv)] {
        let expected = analyze(&Report { text, lang });
        for (name, variant) in VARIANTS {
            assert_eq!(variant(&Report { text, lang }), expected, "{name}");
        }
    }
}

#[test]
fn anchored_applies_to_the_known_layout_only() {
    assert!(anchored_applies(&Report {
        text: EN,
        lang: Lang::En
    }));
    assert!(anchored_applies(&Report {
        text: SV,
        lang: Lang::Sv
    }));
    assert!(!anchored_applies(&Report {
        text: EN,
        lang: Lang::Da
    }));
}

#[test]
fn anchored_falls_back_when_the_first_line_changes() {
    let changed = EN.replace("Third quarter 2026", "Q3 2026 highlights");
    let report = Report {
        text: &changed,
        lang: Lang::En,
    };
    assert!(!anchored_applies(&report));
    let verdict = analyze_anchored(&report);
    assert_eq!(verdict, analyze_aho(&report));
    assert_eq!(verdict, run(EN, Lang::En));
}

#[test]
fn anchored_falls_back_when_a_prefix_or_unit_changes() {
    let renamed = EN.replace("• Operating profit (EBIT)", "• Operating result (EBIT)");
    let report = Report {
        text: &renamed,
        lang: Lang::En,
    };
    assert!(!anchored_applies(&report));
    assert_eq!(analyze_anchored(&report), analyze_aho(&report));

    let wrong_unit = EN.replace("EBIT margin was 11.9% (9.8)", "EBIT margin was 11.9 (9.8)");
    assert!(!anchored_applies(&Report {
        text: &wrong_unit,
        lang: Lang::En
    }));
}

#[test]
fn every_variant_abstains_on_a_foreign_release() {
    let foreign = "Other Company Oyj: Osavuosikatsaus\n\nLiikevaihto oli 41,2 miljoonaa euroa. \
                   Kokouskutsu julkaistaan myöhemmin.";
    for (_, variant) in VARIANTS {
        assert_abstains(variant, foreign, Lang::Fi);
        assert_abstains(variant, "no figures here", Lang::En);
    }
}

#[test]
fn timing_runs_for_every_variant() {
    for (_, variant) in VARIANTS {
        let (p50, p99) = time_per_release(variant, &fixtures(), 50);
        assert!(p50 <= p99);
    }
}
