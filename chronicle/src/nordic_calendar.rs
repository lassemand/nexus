/// Daily MFN financial-calendar refresh (NEX-161, NEX-171).
///
/// For every company with `companies.mfn_slug`, fetches the public MFN company
/// page (HTML only; MFN's JSON/RSS feeds are not used), parses the upcoming
/// reports and publishes them as a `report_calendar` snapshot on
/// `special.events`, unless the calendar is unchanged since the last snapshot
/// (`--force` publishes regardless). signal replaces the company's future
/// scheduled rows with it. A page that cannot be parsed publishes nothing and
/// makes the job exit non-zero so the failure is noticed.
use chronicle_lib::nordic::calendar::{refresh_company, MfnClient, MFN_BASE_URL};
use chronicle_lib::nordic::db;
use chronicle_lib::nordic::events::{KafkaSink, SPECIAL_EVENTS_TOPIC};
use chronicle_lib::registry;
use clap::Parser;
use sqlx::postgres::PgPoolOptions;
use std::process::ExitCode;
use std::time::Duration;
use tracing::{error, info};

#[derive(Parser)]
#[command(about = "Publish upcoming Nordic report dates from each company's MFN calendar")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "MFN_BASE_URL", default_value = MFN_BASE_URL)]
    mfn_base_url: String,

    /// Publish every calendar, even when unchanged since the last snapshot.
    #[arg(long, env = "NORDIC_CALENDAR_FORCE")]
    force: bool,

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
            error!(error = %e, "nordic-calendar failed");
            ExitCode::FAILURE
        }
    }
}

/// Returns `Ok(false)` if any company failed (fetch, parse or publish).
async fn run(args: Args) -> anyhow::Result<bool> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    registry::sync_or_warn(&pool, Some(&args.kafka_brokers), &args.tickers_topic).await;

    let sink = KafkaSink::new(&args.kafka_brokers, &args.special_events_topic)?;
    let companies = db::load_mapped_companies(&pool).await?;
    let mut client = MfnClient::new(args.mfn_base_url, Duration::from_secs(2))?;

    let mut failed = 0;
    for c in &companies {
        match refresh_company(&pool, &mut client, &sink, c, args.force).await {
            Ok(outcome) => info!(ticker = %c.ticker, ?outcome, "calendar checked"),
            Err(e) => {
                failed += 1;
                error!(ticker = %c.ticker, error = %e, "calendar refresh failed");
            }
        }
    }
    info!(companies = companies.len(), failed, "nordic-calendar done");
    Ok(failed == 0)
}
