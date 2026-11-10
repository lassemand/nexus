//! The example kernel's five lookups on English and Swedish fixtures.

use verdict_sample::{
    analyze, analyze_anchored, analyze_automaton, analyze_lowercase, analyze_pattern,
    analyze_window,
};
use verdict_sdk::harness::{assert_abstains, check_fixtures, time_per_release, Fixture};
use verdict_sdk::{run_into, Analyze, Decision, Lang, Report, Verdict};

const EN: &str = include_str!("fixtures/en_interim.txt");
const SV: &str = include_str!("fixtures/sv_interim.txt");

const VARIANTS: [(&str, Analyze); 5] = [
    ("anchored", analyze_anchored),
    ("window", analyze_window),
    ("automaton", analyze_automaton),
    ("pattern", analyze_pattern),
    ("lowercase", analyze_lowercase),
];

fn fixture(name: &'static str, text: &'static str, lang: Lang) -> Fixture<'static> {
    Fixture {
        name,
        text,
        lang,
        expected: vec![
            ("net_sales", Some("812.4"), Some("725.1")),
            ("ebit", Some("96.3"), Some("71.0")),
        ],
        expected_decision: Decision::Buy,
    }
}

fn fixtures() -> [Fixture<'static>; 2] {
    [
        fixture("en_interim.txt", EN, Lang::En),
        fixture("sv_interim.txt", SV, Lang::Sv),
    ]
}

fn run(analyze: Analyze, text: &str, lang: Lang) -> Verdict {
    analyze(&Report { text, lang })
}

#[test]
fn every_variant_gives_the_expected_metrics() {
    for (name, variant) in VARIANTS {
        assert_eq!(
            check_fixtures(variant, &fixtures()),
            Ok(()),
            "variant {name}"
        );
    }
}

#[test]
fn every_variant_gives_the_same_verdict() {
    for fixture in fixtures() {
        let reference = run(analyze, fixture.text, fixture.lang);
        for (name, variant) in VARIANTS {
            assert_eq!(
                run(variant, fixture.text, fixture.lang),
                reference,
                "variant {name} on {}",
                fixture.name
            );
        }
    }
}

#[test]
fn units_and_reason() {
    let en = run(analyze, EN, Lang::En);
    let units: Vec<_> = en.metrics.iter().map(|m| m.unit.as_deref()).collect();
    assert_eq!(units, [Some("million"), Some("million")]);
    assert_eq!(en.reason, "net sales up 12.0 % and EBIT up");
    let sv = run(analyze, SV, Lang::Sv);
    let units: Vec<_> = sv.metrics.iter().map(|m| m.unit.as_deref()).collect();
    assert_eq!(units, [Some("MSEK"), Some("MSEK")]);
}

#[test]
fn anchored_falls_back_when_a_prefix_changes() {
    let changed = EN.replace("• Net sales increased by 12% to", "• Revenues rose 12% to");
    assert!(changed.contains("Revenues rose"));
    let fallback = run(analyze, &changed, Lang::En);
    assert_eq!(fallback, run(analyze_automaton, &changed, Lang::En));
    assert_eq!(fallback.decision, Decision::Buy);
    assert_eq!(fallback.metrics[0].current.as_deref(), Some("812.4"));
    assert_eq!(fallback.metrics[0].prior.as_deref(), Some("725.1"));
}

#[test]
fn anchored_falls_back_when_the_heading_changes() {
    let changed = EN.replace("Third quarter 2026", "Fourth quarter 2026");
    assert_eq!(
        run(analyze, &changed, Lang::En),
        run(analyze_automaton, &changed, Lang::En)
    );
    assert_eq!(run(analyze, &changed, Lang::En).decision, Decision::Buy);
}

#[test]
fn unknown_language_uses_the_fallback() {
    assert_eq!(run(analyze, EN, Lang::Other).decision, Decision::Buy);
}

#[test]
fn loss_narrowing_counts_as_ebit_up() {
    let text = "Net sales amounted to SEK 120.0 million (100.0). \
                Operating profit was SEK -6.1 million (-11.7).";
    for (name, variant) in VARIANTS {
        assert_eq!(
            run(variant, text, Lang::En).decision,
            Decision::Buy,
            "{name}"
        );
    }
}

#[test]
fn both_down_is_no_buy() {
    let text = "Net sales amounted to SEK 90.0 million (100.0). \
                Operating profit was SEK -6.1 million (4.0).";
    let verdict = run(analyze, text, Lang::En);
    assert_eq!(verdict.decision, Decision::NoBuy);
    assert_eq!(verdict.reason, "net sales down 10 % and EBIT down");
}

#[test]
fn mixed_or_zero_prior_abstains() {
    let mixed = "Net sales SEK 120.0 million (100.0). EBIT SEK 3.0 million (4.0).";
    assert_eq!(run(analyze, mixed, Lang::En).decision, Decision::Abstain);
    let zero = "Net sales SEK 12.0 million (0). EBIT SEK 3.0 million (-4.0).";
    let verdict = run(analyze, zero, Lang::En);
    assert_eq!(verdict.reason, "prior net sales is zero");
}

#[test]
fn ebitda_is_not_ebit() {
    let text = "Net sales SEK 120.0 million (100.0). EBITDA SEK 30.0 million (20.0).";
    for (name, variant) in VARIANTS {
        let verdict = run(variant, text, Lang::En);
        assert_eq!(verdict.reason, "EBIT not found", "{name}");
    }
}

#[test]
fn foreign_release_abstains() {
    let text = "Example Group AB appoints new Chief Financial Officer\n\n\
                The board has appointed a new CFO, who will take up the position on 1 March 2027.";
    for (_, variant) in VARIANTS {
        assert_abstains(variant, text, Lang::En);
    }
    assert_eq!(run(analyze, text, Lang::En).reason, "net sales not found");
}

#[test]
fn run_into_matches_direct_call() {
    let mut out = Vec::new();
    run_into(analyze, EN.as_bytes(), Lang::En.code(), &mut out);
    let direct = serde_json::to_vec(&run(analyze, EN, Lang::En)).expect("a verdict serializes");
    assert_eq!(out, direct);
}

#[test]
fn timing_runs_for_every_variant() {
    for (name, variant) in VARIANTS {
        let (p50, p99) = time_per_release(variant, &fixtures(), 20);
        assert!(p50 <= p99, "{name}");
    }
}
