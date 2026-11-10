//! SDK that every analyst report kernel is written against.
//!
//! A kernel is a small crate, compiled to `wasm32-unknown-unknown`, that reads
//! one company's report release and returns a [`Verdict`]. The SDK holds only
//! the ABI ([`run_into`], [`export_kernel!`]), the types ([`Lang`], [`Report`],
//! [`Metric`], [`Decision`], [`Verdict`]) and, behind the `harness` feature, a
//! test and timing harness. How a report is searched and how figures are read
//! is the kernel's own code; see `verdict/AGENT_GUIDE.md`.

mod abi;
mod lang;
mod types;

#[cfg(feature = "harness")]
pub mod harness;

pub use abi::run_into;
pub use lang::Lang;
pub use types::{Analyze, Decision, Metric, Report, Verdict};

pub use rust_decimal::Decimal;
