//! Library surface for `chronicle`'s check logic — exists specifically so
//! `chronicle/tests/smoke_test.rs` can reach it as a genuine integration
//! test. Every other binary in this crate (`chronicle`, `market`, `earnings`,
//! `filings`, `pdmr`, `saxo_stream`) stays exactly as it was, independent of
//! this lib target.

pub mod verify;
