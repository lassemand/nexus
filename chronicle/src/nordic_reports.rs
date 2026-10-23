/// Nasdaq Nordic report-release ingestion binary (NEX-160).
///
/// For every Swedish-calendar company (`FNSE`, `XSTO`) with a
/// `companies.nasdaq_news_company` mapping, pages through the exchange's
/// company-news feed and records each item in `report_releases` (see
/// `chronicle_lib::nordic::db::record_release`): exact release instant,
/// interim/annual classification, corrections and financial-calendar
/// announcements. Reports fill matching scheduled rows from the MFN calendar.
///
/// Incremental: paging stops at the first page containing a known
/// `disclosure_id`. Companies that just announced a calendar change get their
/// MFN calendar refreshed at the end of the run (NEX-161).
use chronicle_lib::nordic::calendar::{MfnClient, MFN_BASE_URL};
use chronicle_lib::nordic::classify::REPORT_CATEGORIES;
use chronicle_lib::nordic::db;
use chronicle_lib::nordic::feed::{NewsClient, NASDAQ_NEWS_URL};
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

    #[arg(long, env = "NASDAQ_NEWS_URL", default_value = NASDAQ_NEWS_URL)]
    news_url: String,

    #[arg(long, env = "MFN_BASE_URL", default_value = MFN_BASE_URL)]
    mfn_base_url: String,
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

/// Returns `Ok(false)` only if every attempted company failed.
async fn run(args: Args) -> anyhow::Result<bool> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;

    info!(categories = ?REPORT_CATEGORIES, "report categories counted");
    let companies = db::load_companies(&pool, SE_MICS).await?;
    let mut news = NewsClient::new(
        args.news_url,
        Duration::from_secs(1),
        Duration::from_secs(2),
    )?;
    let mut mfn = MfnClient::new(args.mfn_base_url, Duration::from_secs(2))?;

    let summary = process_companies(&pool, &mut news, &mut mfn, &companies).await;
    info!(
        attempted = summary.attempted,
        failed = summary.failed,
        calendars_refreshed = ?summary.calendars_refreshed,
        "nordic-reports done"
    );
    Ok(summary.is_success())
}
