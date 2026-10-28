//! Nordic report ingestion: past reports from MFN's company news feed and
//! upcoming ones from the MFN calendar, published as special events (NEX-171).
//! signal stores them; chronicle keeps only its read position.
use model::asset::mic;

/// Every exchange the Nordic jobs cover: First North Stockholm, Nasdaq
/// Stockholm, Nasdaq Copenhagen and First North Denmark. One list for
/// `nordic-reports`; `nordic-calendar` and `nordic-live` take every company
/// that has an `mfn_slug`, whatever its MIC.
pub const NORDIC_MICS: &[&str] = &[mic::FNSE, mic::XSTO, mic::XCSE, mic::FNDK];

pub mod calendar;
pub mod classify;
pub mod db;
pub mod events;
pub mod feed;
pub mod ingest;
pub mod live;
pub mod mar_release;
