/// Daily MFN financial-calendar refresh (NEX-161).
///
/// For every company with `companies.mfn_slug`, fetches the public MFN company
/// page (HTML only; MFN's JSON/RSS feeds are not used) and replaces the
/// company's scheduled (unreleased, future) rows in `report_releases`.
/// A page that cannot be parsed keeps the existing rows and makes the job exit
/// non-zero so the failure is noticed.
use chronicle_lib::nordic::calendar::{refresh_company, MfnClient, Refresh, MFN_BASE_URL};
use chronicle_lib::registry;
use clap::Parser;
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::process::ExitCode;
use std::time::Duration;
use tracing::{error, info};

#[derive(Parser)]
#[command(about = "Refresh upcoming Nordic report dates from each company's MFN calendar")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "MFN_BASE_URL", default_value = MFN_BASE_URL)]
    mfn_base_url: String,

    /// Brokers for the `company.tickers` sync; without it the companies table
    /// is used as it is.
    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: Option<String>,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = registry::TICKERS_TOPIC)]
    tickers_topic: String,
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

/// Returns `Ok(false)` if any company failed (fetch, parse or database).
async fn run(args: Args) -> anyhow::Result<bool> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    registry::sync_or_warn(&pool, args.kafka_brokers.as_deref(), &args.tickers_topic).await;

    let companies = sqlx::query(
        "SELECT ticker, exchange_mic FROM companies WHERE mfn_slug IS NOT NULL ORDER BY ticker",
    )
    .fetch_all(&pool)
    .await?;
    let mut client = MfnClient::new(args.mfn_base_url, Duration::from_secs(2))?;

    let mut failed = 0;
    for row in &companies {
        let (ticker, mic): (String, String) = (row.get("ticker"), row.get("exchange_mic"));
        match refresh_company(&pool, &mut client, &ticker, &mic).await {
            Ok(Refresh::Refreshed(_)) | Ok(Refresh::NoSlug) => {}
            Err(e) => {
                failed += 1;
                error!(ticker = %ticker, error = %e, "calendar refresh failed");
            }
        }
    }
    info!(companies = companies.len(), failed, "nordic-calendar done");
    Ok(failed == 0)
}
