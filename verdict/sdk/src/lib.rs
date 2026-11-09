//! SDK that every analyst report kernel is written against.
//!
//! A kernel is a small crate, compiled to `wasm32-unknown-unknown`, that reads
//! one company's report release and returns a [`Verdict`]. It finds every label
//! in one pass with [`Labels`], reads figures with [`next_number`] and
//! [`current_and_prior`], and exports the WASM ABI with [`export_kernel!`].
//! The rules a kernel must follow are in `verdict/AGENT_GUIDE.md`.

mod abi;
mod labels;
mod lang;
mod number;
mod types;

pub use abi::run_into;
pub use labels::{left_boundary, right_boundary, Hit, Labels};
pub use lang::{Lang, NumberStyle};
pub use number::{current_and_prior, next_number, unit_after, Num};
pub use types::{Analyze, Decision, Metric, Report, Verdict};

pub use aho_corasick;
pub use memchr;
pub use regex;
pub use rust_decimal::Decimal;
