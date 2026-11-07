/// Daily MFN financial-calendar refresh (NEX-161, NEX-171).
///
/// For every company with `companies.mfn_slug`, fetches the public MFN company
/// page (HTML only; MFN's JSON/RSS feeds are not used), parses the upcoming
/// reports and publishes them as a `report_calendar` snapshot on
/// `special.events`, unless the calendar is unchanged since the last snapshot
/// (`--force` publishes regardless). signal replaces the company's future
/// scheduled rows with it. A page that cannot be parsed publishes nothing and
/// makes the job exit non-zero so the failure is noticed.
use chronicle_lib::nordic::calendar::{refresh_company, MfnClient, Refresh, MFN_BASE_URL};
use chronicle_lib::nordic::db;
use chronicle_lib::nordic::events::{KafkaSink, SPECIAL_EVENTS_TOPIC};
use chronicle_lib::nordic::ingest::scan_calendar_releases;
use chronicle_lib::registry;
use chrono::Utc;
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

    /// Before refreshing, page each company's MFN news feed back this many
    /// months (default 18) and publish a `report_calendar_release` event for
    /// every calendar release found.
    #[arg(
        long,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "18",
        value_name = "MONTHS"
    )]
    scan_releases: Option<u32>,

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
    match run_job(Args::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            error!(error = %e, "nordic-calendar failed");
            ExitCode::FAILURE
        }
    }
}

/// Job name in the Pushgateway.
const JOB: &str = "nordic-calendar";

/// Runs the job and pushes its metrics, on success and on error alike. A partial
/// failure (`Ok(false)`) is a successful run for the metrics; the exit code still
/// says failed.
async fn run_job(args: Args) -> anyhow::Result<bool> {
    telemetry::run_batch(JOB, run(args)).await
}

/// Counts one company in `nordic_calendar_companies_total{outcome}`.
fn count_company(outcome: &'static str) {
    metrics::counter!("nordic_calendar_companies_total", "outcome" => outcome).increment(1);
}

/// Returns `Ok(false)` if any company failed (fetch, parse or publish).
async fn run(args: Args) -> anyhow::Result<bool> {
    for outcome in ["published", "unchanged", "no_slug", "failed"] {
        metrics::counter!("nordic_calendar_companies_total", "outcome" => outcome).absolute(0);
    }
    let mut upcoming = 0usize;
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
        if let (Some(months), Some(slug)) = (args.scan_releases, c.mfn_slug.as_deref()) {
            let from = Utc::now() - chrono::Months::new(months);
            match scan_calendar_releases(&mut client, &sink, c, slug, from).await {
                Ok((sent, pages)) => {
                    info!(ticker = %c.ticker, sent, pages, "calendar releases scanned")
                }
                Err(e) => {
                    failed += 1;
                    count_company("failed");
                    error!(ticker = %c.ticker, error = %e, "calendar release scan failed");
                    continue;
                }
            }
        }
        match refresh_company(&pool, &mut client, &sink, c, args.force).await {
            Ok(outcome) => {
                match outcome {
                    Refresh::Published(n) => {
                        upcoming += n;
                        count_company("published");
                    }
                    Refresh::Unchanged => count_company("unchanged"),
                    Refresh::NoSlug => count_company("no_slug"),
                }
                info!(ticker = %c.ticker, ?outcome, "calendar checked")
            }
            Err(e) => {
                failed += 1;
                count_company("failed");
                error!(ticker = %c.ticker, error = %e, "calendar refresh failed");
            }
        }
    }
    metrics::gauge!("nordic_calendar_upcoming_reports").set(upcoming as f64);
    info!(
        companies = companies.len(),
        failed, upcoming, "nordic-calendar done"
    );
    Ok(failed == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A forced error (unusable database URL) still pushes the run, as a failure.
    #[tokio::test]
    async fn a_failed_run_still_pushes_its_metrics() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        std::env::set_var("PUSHGATEWAY_URL", server.uri());
        let args = Args::parse_from([
            "nordic-calendar",
            "--database-url",
            "not-a-url",
            "--kafka-brokers",
            "none:1",
        ]);
        assert!(run_job(args).await.is_err());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/metrics/job/nordic-calendar");
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains("nexus_job_last_run_success{job=\"nordic-calendar\"} 0"));
    }
}
