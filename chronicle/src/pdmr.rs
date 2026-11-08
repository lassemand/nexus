/// FI (Finansinspektionen) PDMR insider-transaction ingestion binary.
///
/// Fetches MAR Article 19 PDMR disclosures from Sweden's financial regulator
/// and publishes them as `UnifiedInsiderTransaction` (NEX-92) to Kafka.
///
/// Mirrors `chronicle/src/filings.rs` structurally (SEC Form 4 path) — both
/// publish to the shared `insider.transactions` topic using the unified schema.
///
/// Tickers are read from the `companies` Postgres table filtered by
/// `exchange_mic = 'FNSE'` — no hardcoded tickers. The table is first synced
/// from `company.tickers` ([`chronicle_lib::registry`]), and a company's
/// `fi_issuer` column, when set, is the FI issuer name searched for.
///
/// # Data boundary
///
/// Pre-July-2016 (pre-MAR) FI data unavailable; `from` clamped to 2016-07-03.
///
/// # Correction handling
///
/// FI corrections: `Status != "Aktuell"` rows are filtered before publishing.
/// Corrections are published with `is_amendment = true` and a stable
/// `amended_transaction_id` (via `model::insider::transaction_id`) so
/// log-compacted consumers overwrite the original.
mod db;
mod kafka;

use anyhow::Context;
use chronicle_lib::insider_source::{fi_register::MAR_START, insider_source, SourceContext};
use chronicle_lib::nordic::db::Company;
use chronicle_lib::registry;
use chrono::{Duration, NaiveDate, Utc};
use clap::Parser;
use kafka::ChronicleProducer;
use model::asset::mic;
use model::nordic;
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::process::ExitCode;
use tracing::{error, info, warn};

// ── Args ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Fetch FI PDMR insider transactions and publish as InsiderTransaction to Kafka")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    // Not `KAFKA_TOPIC`: chronicle-secret sets that to `earnings.calendar` for
    // every chronicle job. Same variable signal consumes with.
    #[arg(
        long,
        env = "INSIDER_TRANSACTIONS_TOPIC",
        default_value = "insider.transactions"
    )]
    kafka_topic: String,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = registry::TICKERS_TOPIC)]
    tickers_topic: String,

    #[arg(long, env = "LOOKBACK_DAYS", default_value = "90")]
    lookback_days: i64,
}

// ── Registry query ────────────────────────────────────────────────────────

// FIXME(pdmr-xsto, no bug id yet): only `FNSE` companies are loaded, so Swedish
// main-market (`XSTO`) companies get no insider data. Kept as is (NEX-211).
/// Load all FNSE-tagged tickers, with their FI issuer name when mapped.
async fn load_fnse_companies(pool: &sqlx::PgPool) -> anyhow::Result<Vec<(String, Option<String>)>> {
    let rows = sqlx::query(
        "SELECT ticker, fi_issuer FROM companies WHERE exchange_mic = 'FNSE' ORDER BY ticker",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| (r.get("ticker"), r.get("fi_issuer")))
        .collect())
}

// ── Main ──────────────────────────────────────────────────────────────────

/// Job name in the Pushgateway. Run metrics: `pdmr_rows_fetched_total` (FI register
/// rows, one transaction each), `pdmr_transactions_published_total`,
/// `pdmr_parse_errors_total` (rows skipped for an unreadable date).
const JOB: &str = "pdmr";

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();
    match run_job(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %format!("{e:#}"), "pdmr failed");
            ExitCode::FAILURE
        }
    }
}

/// Runs the job and pushes its metrics, on success and on error alike.
async fn run_job(args: Args) -> anyhow::Result<()> {
    telemetry::run_batch(JOB, run(args)).await
}

async fn run(args: Args) -> anyhow::Result<()> {
    for name in [
        "pdmr_rows_fetched_total",
        "pdmr_transactions_published_total",
        "pdmr_parse_errors_total",
    ] {
        metrics::counter!(name).absolute(0);
    }
    // Connect to DB and run migrations so the companies table is up-to-date.
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&args.database_url)
        .await
        .context("failed to connect to postgres")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("migrations failed")?;

    registry::sync_or_warn(&pool, Some(&args.kafka_brokers), &args.tickers_topic).await;

    // Load FNSE-tagged tickers from the extended companies registry.
    let tickers = match load_fnse_companies(&pool).await {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "failed to load FNSE tickers from companies table");
            return Err(e.context("failed to load FNSE tickers from companies table"));
        }
    };

    if tickers.is_empty() {
        warn!(
            "no FNSE tickers registered — register tickers via: \
             nexus register <TICKER> --exchange-mic FNSE"
        );
        return Ok(());
    }

    info!(
        count = tickers.len(),
        "loaded FNSE tickers from companies table"
    );

    let client = reqwest::Client::builder()
        .user_agent("nexus lasse.alm@gsfleet.io")
        .build()
        .context("failed to build HTTP client")?;

    let producer =
        ChronicleProducer::new(&args.kafka_brokers).context("failed to connect to Kafka")?;

    let to = Utc::now().date_naive();
    let mar_start = NaiveDate::parse_from_str(MAR_START, "%Y-%m-%d")?;
    // Pre-MAR data is unavailable — clamp from to 2016-07-03.
    let from = (to - Duration::days(args.lookback_days)).max(mar_start);

    // The source comes from the venue's country, not from a country code here.
    let Some(source) =
        nordic::country_for_mic(mic::FNSE).and_then(|c| insider_source(c.insider_source))
    else {
        anyhow::bail!("{} has no insider source", mic::FNSE);
    };

    for (i, (ticker, fi_issuer)) in tickers.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        info!(ticker, source = source.name(), from = %from, to = %to, "fetching insider transactions");
        let company = Company {
            ticker: ticker.clone(),
            exchange_mic: mic::FNSE.to_string(),
            mfn_slug: None,
            name: None,
            isin: None,
        };
        let mut ctx = SourceContext {
            http: &client,
            feed: None,
            fi_issuer: fi_issuer.as_deref(),
        };
        let transactions = match source.fetch(&mut ctx, &company, from).await {
            Ok(t) => t,
            Err(e) => {
                error!(ticker, error = %e, "failed to fetch insider transactions");
                continue;
            }
        };

        info!(
            ticker,
            count = transactions.len(),
            "fetched PDMR transactions"
        );
        metrics::counter!("pdmr_rows_fetched_total").increment(transactions.len() as u64);

        for txn in transactions {
            if let Err(e) = producer
                .publish_unified_insider_transaction(&args.kafka_topic, txn.clone())
                .await
            {
                error!(ticker, error = %e, "failed to publish UnifiedInsiderTransaction");
            } else {
                metrics::counter!("pdmr_transactions_published_total").increment(1);
                info!(
                    ticker,
                    pdmr = %txn.person_name,
                    txn_type = txn.transaction_type,
                    volume = txn.volume,
                    price = txn.price_per_unit,
                    currency = %txn.currency,
                    amendment = txn.is_amendment,
                    "published UnifiedInsiderTransaction (FI)"
                );
            }
        }
    }
    Ok(())
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
            "pdmr",
            "--database-url",
            "not-a-url",
            "--kafka-brokers",
            "none:1",
        ]);
        assert!(run_job(args).await.is_err());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/metrics/job/pdmr");
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains("nexus_job_last_run_success{job=\"pdmr\"} 0"));
    }
}
