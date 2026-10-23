/// Nordic report-release ingestion binary (NEX-160, NEX-166).
///
/// For every Swedish-calendar company (`FNSE`, `XSTO`) with a
/// `companies.mfn_slug`, pages through MFN's JSON news feed
/// (`https://mfn.se/all/a/<slug>.json`, fetched despite `robots.txt` with the
/// owner's approval) and records each item in `report_releases` (see
/// `chronicle_lib::nordic::db::record_release`): exact release instant,
/// interim/annual classification, corrections and financial-calendar
/// announcements. Reports fill matching scheduled rows from the MFN calendar.
///
/// Incremental by default; `--backfill` pages to the end of each feed.
/// Companies that just announced a calendar change get their MFN calendar
/// refreshed at the end of the run (NEX-161). Exits non-zero if any company
/// failed.
use chronicle_lib::nordic::calendar::{MfnClient, MFN_BASE_URL};
use chronicle_lib::nordic::db;
use chronicle_lib::nordic::ingest::process_companies;
use clap::Parser;
use sqlx::postgres::PgPoolOptions;
use std::process::ExitCode;
use std::time::Duration;
use tracing::{error, info};

/// Exchanges following the Swedish calendar / Stockholm time.
const SE_MICS: &[&str] = &["FNSE", "XSTO"];

#[derive(Parser)]
#[command(about = "Ingest past Nordic report releases from the Nasdaq Nordic company-news API")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "MFN_BASE_URL", default_value = MFN_BASE_URL)]
    mfn_base_url: String,

    /// Page every company's feed to the end instead of stopping when caught up.
    #[arg(long, env = "NORDIC_BACKFILL")]
    backfill: bool,
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

    let companies = db::load_companies(&pool, SE_MICS).await?;
    let mut feed = MfnClient::new(args.mfn_base_url.clone(), Duration::from_secs(1))?;
    let mut calendar = MfnClient::new(args.mfn_base_url, Duration::from_secs(2))?;

    let summary =
        process_companies(&pool, &mut feed, &mut calendar, &companies, args.backfill).await;
    info!(
        attempted = summary.attempted,
        failed = summary.failed,
        calendars_refreshed = ?summary.calendars_refreshed,
        "nordic-reports done"
    );
    Ok(summary.is_success())
}
