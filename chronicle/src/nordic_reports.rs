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
use chronicle_lib::nordic::ingest::{
    describe_run_metrics, process_companies, process_insiders_only,
};
use chronicle_lib::nordic::insiders::{mar_start, KafkaInsiderSink, INSIDER_TRANSACTIONS_TOPIC};
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

    /// Topic signal consumes insider transactions from.
    #[arg(long, env = "INSIDER_TRANSACTIONS_TOPIC", default_value = INSIDER_TRANSACTIONS_TOPIC)]
    insider_transactions_topic: String,

    /// Only re-read Finnish companies' feeds for managers' transactions and
    /// publish those. No report events, and no cursor is read or moved. Use it
    /// for history (`--from`) or to re-send; it is idempotent.
    #[arg(long, env = "NORDIC_INSIDERS_ONLY")]
    insiders_only: bool,

    /// With `--insiders-only`: the oldest release to read (YYYY-MM-DD, UTC).
    /// MAR started on 2016-07-03; earlier releases are ignored anyway.
    #[arg(long, env = "NORDIC_INSIDERS_FROM")]
    from: Option<chrono::NaiveDate>,

    /// Only this ticker (with `--insiders-only`).
    #[arg(long, env = "NORDIC_TICKER")]
    ticker: Option<String>,

    /// Only this MIC (with `--insiders-only`), e.g. `XHEL`.
    #[arg(long, env = "NORDIC_MIC")]
    mic: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();
    match run_job(Args::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            error!(error = %e, "nordic-reports failed");
            ExitCode::FAILURE
        }
    }
}

/// Job name in the Pushgateway for a run of this kind.
///
/// Manual runs (`--backfill`, `--insiders-only`) push as `nordic-reports-backfill`,
/// so they never overwrite the daily job's last-success time.
fn job_name(args: &Args) -> &'static str {
    if args.backfill || args.insiders_only {
        "nordic-reports-backfill"
    } else {
        "nordic-reports"
    }
}

/// Runs the job and pushes its metrics, on success and on error alike. A partial
/// failure (`Ok(false)`) is a successful run for the metrics; the exit code still
/// says failed.
async fn run_job(args: Args) -> anyhow::Result<bool> {
    let job = job_name(&args);
    telemetry::run_batch(job, run(args)).await
}

/// Returns `Ok(false)` if any attempted company failed.
async fn run(args: Args) -> anyhow::Result<bool> {
    describe_run_metrics();
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    registry::sync_or_warn(&pool, Some(&args.kafka_brokers), &args.tickers_topic).await;

    let sink = KafkaSink::new(&args.kafka_brokers, &args.special_events_topic)?;
    let insider_sink =
        KafkaInsiderSink::new(&args.kafka_brokers, &args.insider_transactions_topic)?;
    let mut companies = db::load_companies(&pool, NORDIC_MICS).await?;
    let mut feed = MfnClient::new(args.mfn_base_url.clone(), Duration::from_secs(1))?;

    if args.insiders_only {
        companies.retain(|c| {
            args.ticker
                .as_deref()
                .is_none_or(|t| c.ticker.eq_ignore_ascii_case(t))
                && args
                    .mic
                    .as_deref()
                    .is_none_or(|m| c.exchange_mic.eq_ignore_ascii_case(m))
        });
        let from = args.from.unwrap_or_else(mar_start);
        let from = from.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
        let summary = process_insiders_only(&mut feed, &insider_sink, &companies, from).await;
        info!(
            attempted = summary.attempted,
            failed = summary.failed,
            insiders_published = summary.insiders_published,
            mar_parse_errors = summary.mar_parse_errors,
            mar_suspicious_zero_price = summary.mar_suspicious_zero_price,
            "nordic-reports --insiders-only done"
        );
        return Ok(summary.is_success());
    }
    let mut calendar = MfnClient::new(args.mfn_base_url, Duration::from_secs(2))?;

    let summary = process_companies(
        &pool,
        &mut feed,
        &mut calendar,
        &sink,
        &insider_sink,
        &companies,
        args.backfill,
    )
    .await;
    info!(
        attempted = summary.attempted,
        failed = summary.failed,
        insiders_published = summary.insiders_published,
        mar_parse_errors = summary.mar_parse_errors,
        mar_suspicious_zero_price = summary.mar_suspicious_zero_price,
        calendars_refreshed = ?summary.calendars_refreshed,
        "nordic-reports done"
    );
    Ok(summary.is_success())
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
            "nordic-reports",
            "--database-url",
            "not-a-url",
            "--kafka-brokers",
            "none:1",
        ]);
        assert!(run_job(args).await.is_err());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/metrics/job/nordic-reports");
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains("nexus_job_last_run_success{job=\"nordic-reports\"} 0"));
    }

    #[test]
    fn manual_runs_push_under_their_own_job() {
        let daily = Args::parse_from([
            "nordic-reports",
            "--database-url",
            "x",
            "--kafka-brokers",
            "x",
        ]);
        assert_eq!(job_name(&daily), "nordic-reports");
        let backfill = Args::parse_from([
            "nordic-reports",
            "--database-url",
            "x",
            "--kafka-brokers",
            "x",
            "--backfill",
        ]);
        assert_eq!(job_name(&backfill), "nordic-reports-backfill");
        let insiders = Args::parse_from([
            "nordic-reports",
            "--database-url",
            "x",
            "--kafka-brokers",
            "x",
            "--insiders-only",
        ]);
        assert_eq!(job_name(&insiders), "nordic-reports-backfill");
    }
}
