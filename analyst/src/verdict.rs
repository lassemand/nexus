use serde::{Deserialize, Serialize};
use verdict_sdk::Decision;

/// One metric in a kernel's verdict, as the analyst reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelMetric {
    /// The metric's name.
    pub key: String,
    /// This period's figure, as an exact decimal string.
    pub current: Option<String>,
    /// The prior period's figure.
    pub prior: Option<String>,
    /// What was expected, if the kernel knows.
    pub expected: Option<String>,
    /// The unit after the figure.
    pub unit: Option<String>,
}

/// A kernel's answer, parsed from its JSON output. Same shape as
/// `verdict_sdk::Verdict`, with the keys as owned strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelVerdict {
    /// The decision.
    pub decision: Decision,
    /// Why, in one short sentence.
    pub reason: String,
    /// Every metric the kernel found.
    pub metrics: Vec<KernelMetric>,
}
