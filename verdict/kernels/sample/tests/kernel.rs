//! The skeleton kernel's contract: it always abstains and its JSON is stable.

use verdict_sample::analyze;
use verdict_sdk::harness::assert_abstains;
use verdict_sdk::{run_into, Decision, Lang, Report};

#[test]
fn abstains_on_any_release() {
    assert_abstains(analyze, "any text at all", Lang::Other);
    assert_abstains(analyze, "", Lang::En);
}

#[test]
fn verdict_json_is_stable() {
    let mut out = Vec::new();
    run_into(analyze, b"text", Lang::Other.code(), &mut out);
    assert_eq!(
        std::str::from_utf8(&out).expect("json is UTF-8"),
        r#"{"decision":"abstain","reason":"no lookup defined","metrics":[]}"#
    );
    let report = Report {
        text: "text",
        lang: Lang::Other,
    };
    assert_eq!(analyze(&report).decision, Decision::Abstain);
}
