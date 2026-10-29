/// Nordic report-release publisher (NEX-160, NEX-166, NEX-171).
///
/// For every Nordic company (`NORDIC_MICS`: `FNSE`, `XSTO`, `XCSE`, `FNDK`, `XHEL`, `FNFI`) with a
/// `companies.mfn_slug`, pages through MFN's JSON news feed
/// (`https://mfn.se/all/a/<slug>.json`, fetched despite `robots.txt` with the
/// owner's approval), classifies each item and publishes reports and
/// corrections as `SpecialEvent`s on `special.events`; signal stores them.
/// chronicle keeps only its read position (`mfn_ingest_state`).
///
/// Incremental from the feed cursor by default; `--backfill` pages to the end
/// of each feed. Companies that announced a calendar change get their MFN
/// calendar refreshed at the end of the run. Exits non-zero if any company
/// failed.
use chronicle_lib::nordic::calendar::{MfnClient, MFN_BASE_URL};
use chronicle_lib::nordic::db;
use chronicle_lib::nordic::events::{KafkaSink, SPECIAL_EVENTS_TOPIC};
use chronicle_lib::nordic::ingest::process_companies;
use chronicle_lib::nordic::NORDIC_MICS;
use chronicle_lib::registry;
use clap::Parser;
use sqlx::postgres::PgPoolOptions;
use std::process::ExitCode;
use std::time::Duration;
use tracing::{error, info};

#[derive(Parser)]
#[command(about = "Publish Nordic report releases from MFN's news feed as special events")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "MFN_BASE_URL", default_value = MFN_BASE_URL)]
    mfn_base_url: String,

    /// Page every company's feed to the end, ignoring the stored cursor.
    #[arg(long, env = "NORDIC_BACKFILL")]
    backfill: bool,

    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = registry::TICKERS_TOPIC)]
    tickers_topic: String,

    /// Topic signal consumes special events from.
    #[arg(long, env = "SPECIAL_EVENTS_TOPIC", default_value = SPECIAL_EVENTS_TOPIC)]
    special_events_topic: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();
    match run(Args::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            error!(error = %e, "nordic-reports failed");
            ExitCode::FAILURE
        }
    }
}

/// Returns `Ok(false)` if any attempted company failed.
async fn run(args: Args) -> anyhow::Result<bool> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    registry::sync_or_warn(&pool, Some(&args.kafka_brokers), &args.tickers_topic).await;

    let sink = KafkaSink::new(&args.kafka_brokers, &args.special_events_topic)?;
    let companies = db::load_companies(&pool, NORDIC_MICS).await?;
    let mut feed = MfnClient::new(args.mfn_base_url.clone(), Duration::from_secs(1))?;
    let mut calendar = MfnClient::new(args.mfn_base_url, Duration::from_secs(2))?;

    let summary = process_companies(
        &pool,
        &mut feed,
        &mut calendar,
        &sink,
        &companies,
        args.backfill,
    )
    .await;
    info!(
        attempted = summary.attempted,
        failed = summary.failed,
        calendars_refreshed = ?summary.calendars_refreshed,
        "nordic-reports done"
    );
    Ok(summary.is_success())
}
