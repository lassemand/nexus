use serde::{Deserialize, Serialize};

use crate::Lang;

/// The release a kernel analyses.
#[derive(Debug, Clone, Copy)]
pub struct Report<'a> {
    /// The release text: title, blank line, MFN `content.text`.
    pub text: &'a str,
    /// The release's language.
    pub lang: Lang,
}

impl<'a> Report<'a> {
    /// The release text as bytes; offsets from [`crate::Labels`] and
    /// [`crate::next_number`] index into it.
    pub fn bytes(&self) -> &'a [u8] {
        self.text.as_bytes()
    }
}

/// One metric a kernel read from the release. Values are exact decimal strings
/// (`Decimal::to_string`), never rounded floats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Metric {
    /// Stable metric name, e.g. `net_sales`.
    pub key: &'static str,
    /// This period's figure.
    pub current: Option<String>,
    /// The same period last year (the figure in parentheses).
    pub prior: Option<String>,
    /// What was expected, if the kernel knows it.
    pub expected: Option<String>,
    /// The unit after the current figure, e.g. `MSEK`, `million`, `%`.
    pub unit: Option<String>,
}

impl Metric {
    /// A metric with only its key set.
    pub fn new(key: &'static str) -> Metric {
        Metric {
            key,
            current: None,
            prior: None,
            expected: None,
            unit: None,
        }
    }
}

/// What the kernel decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Open a simulated position.
    Buy,
    /// The release is bad enough not to buy.
    NoBuy,
    /// The kernel cannot decide (missing metrics, unexpected text, mixed result).
    Abstain,
}

/// A kernel's answer for one release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    /// The decision.
    pub decision: Decision,
    /// Why, in one short sentence.
    pub reason: String,
    /// Every metric the kernel found.
    pub metrics: Vec<Metric>,
}

impl Verdict {
    /// An [`Decision::Abstain`] verdict with no metrics.
    pub fn abstain(reason: impl Into<String>) -> Verdict {
        Verdict {
            decision: Decision::Abstain,
            reason: reason.into(),
            metrics: Vec::new(),
        }
    }
}

/// The function a kernel implements.
pub type Analyze = fn(&Report) -> Verdict;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_is_snake_case() {
        let json = serde_json::to_string(&[Decision::Buy, Decision::NoBuy, Decision::Abstain])
            .expect("serializing an enum cannot fail");
        assert_eq!(json, r#"["buy","no_buy","abstain"]"#);
    }

    #[test]
    fn report_bytes() {
        let report = Report {
            text: "abc",
            lang: Lang::En,
        };
        assert_eq!(report.bytes(), b"abc");
    }
}
