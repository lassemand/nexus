//! Nordic report ingestion: past reports from MFN's company news feed and
//! upcoming ones from the MFN calendar, published as special events (NEX-171).
//! signal stores them; chronicle keeps only its read position.
pub mod calendar;
pub mod classify;
pub mod db;
pub mod events;
pub mod feed;
pub mod ingest;
