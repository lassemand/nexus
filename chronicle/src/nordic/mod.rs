//! Nordic report ingestion: past reports from the Nasdaq Nordic news feed
//! (NEX-160) and scheduled reports from the MFN calendar (NEX-161), both stored
//! in `report_releases`.
pub mod calendar;
pub mod classify;
pub mod db;
pub mod feed;
pub mod ingest;
