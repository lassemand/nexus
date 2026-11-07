mod db;
mod earnings_db;
mod kafka;

use alpha::{PolygonEarningsError, PolygonEarningsProvider};
use anyhow::Context;
use chronicle_lib::job_metrics;
use chrono::{Duration, NaiveDateTime, NaiveTime, TimeZone, Utc};
use clap::Parser;
use earnings_db::{load_unpublished_earnings, mark_earnings_published, upsert_earnings_date};
use kafka::{load_tickers, ChronicleProducer};
use model::{
    asset::Asset,
    generated::{EarningsEvent, SpecialEvent},
};
use sqlx::postgres::PgPoolOptions;
use std::process::ExitCode;
use tracing::{error, info, warn};

/// Fetch quarterly earnings from Polygon and publish them to Kafka.
#[derive(Parser)]
#[command(about = "Fetch quarterly earnings from Polygon and publish as EarningsEvent to Kafka")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    kafka_brokers: String,

    #[arg(long, env = "KAFKA_TOPIC", default_value = "earnings.calendar")]
    kafka_topic: String,

    #[arg(long, env = "POLYGON_API_KEY")]
    polygon_api_key: String,

    #[arg(long, env = "LOOKBACK_YEARS", default_value = "10")]
    lookback_years: u32,

    #[arg(long, env = "SPECIAL_EVENTS_TOPIC", default_value = "special.events")]
    special_events_topic: String,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
    tickers_topic: String,
}

/// Job name in the Pushgateway and the prefix of the run metrics. Units: `items` are quarterly earnings records fetched from Polygon, `events` are `EarningsEvent`s published.
const JOB: &str = "earnings";

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();
    match run_job(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %format!("{e:#}"), "earnings failed");
            ExitCode::FAILURE
        }
    }
}

/// Runs the job and pushes its metrics, on success and on error alike.
async fn run_job(args: Args) -> anyhow::Result<()> {
    telemetry::run_batch(JOB, run(args)).await
}

async fn run(args: Args) -> anyhow::Result<()> {
    job_metrics::register(JOB);
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&args.database_url)
        .await
        .context("failed to connect to postgres")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("migrations failed")?;

    let tickers = load_tickers(&args.kafka_brokers, &args.tickers_topic);

    if tickers.is_empty() {
        warn!("no tickers registered — publish a TickerRegistration to the company.tickers topic first");
        return Ok(());
    }

    info!(count = tickers.len(), "loaded tickers from kafka");

    let polygon = PolygonEarningsProvider::new(&args.polygon_api_key);
    let producer =
        ChronicleProducer::new(&args.kafka_brokers).context("failed to connect to Kafka")?;

    let from_date = Utc::now().date_naive() - Duration::days(365 * args.lookback_years as i64);

    for (i, ticker) in tickers.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(12)).await;
        }

        let asset = Asset::new(ticker);

        info!(ticker = %ticker, from = %from_date, "fetching quarterly earnings");

        let earnings = match polygon
            .quarterly_earnings_with_retry(&asset, from_date)
            .await
        {
            Ok(e) => e,
            Err(PolygonEarningsError::RateLimited { .. }) => {
                error!(ticker = %ticker, "retry failed after rate limit, skipping ticker");
                job_metrics::api_error(JOB, "4xx");
                continue;
            }
            Err(e) => {
                error!(ticker = %ticker, error = %e, "failed to fetch earnings, skipping ticker");
                job_metrics::api_error(
                    JOB,
                    match &e {
                        PolygonEarningsError::Http(e) => job_metrics::class_of_reqwest(e),
                        PolygonEarningsError::Api { status, .. } => {
                            job_metrics::class_of_status(*status)
                        }
                        _ => "other",
                    },
                );
                continue;
            }
        };

        info!(ticker = %ticker, count = earnings.len(), "fetched quarterly earnings");
        job_metrics::items_fetched(JOB, earnings.len());

        for eq in &earnings {
            let record = earnings_db::EarningsRecord {
                ticker: eq.ticker.clone(),
                fiscal_year: eq.fiscal_year as i16,
                fiscal_quarter: eq.fiscal_quarter as i16,
                period_end: eq.period_end,
                filing_date: eq.filing_date,
                eps_actual: eq.eps_actual,
                revenue_actual: eq.revenue_actual,
            };
            if let Err(e) = upsert_earnings_date(&pool, &record).await {
                error!(ticker = %ticker, fiscal_year = eq.fiscal_year, fiscal_quarter = eq.fiscal_quarter, error = %e, "failed to upsert earnings record");
            }
        }

        let unpublished = match load_unpublished_earnings(&pool, ticker).await {
            Ok(rows) => rows,
            Err(e) => {
                error!(ticker = %ticker, error = %e, "failed to load unpublished earnings");
                continue;
            }
        };

        for record in &unpublished {
            let announced_at_unix_secs = naive_date_to_unix_secs(record.filing_date);
            let report_period_unix_secs = naive_date_to_unix_secs(record.period_end);

            let event = EarningsEvent {
                ticker: record.ticker.clone(),
                announced_at_unix_secs,
                report_period_unix_secs,
                fiscal_quarter: record.fiscal_quarter as u32,
                fiscal_year: record.fiscal_year as u32,
                eps_actual: record.eps_actual.unwrap_or(0.0),
                eps_estimate: 0.0,
                revenue_actual: record.revenue_actual.unwrap_or(0.0),
                revenue_estimate: 0.0,
                cik: 0,
                filing_url: String::new(),
            };

            match producer.publish(&args.kafka_topic, ticker, &event).await {
                Ok(()) => {
                    job_metrics::events_published(JOB, 1);
                    if let Err(e) = mark_earnings_published(
                        &pool,
                        ticker,
                        record.fiscal_year,
                        record.fiscal_quarter,
                    )
                    .await
                    {
                        error!(
                            ticker = %ticker,
                            fiscal_year = record.fiscal_year,
                            fiscal_quarter = record.fiscal_quarter,
                            error = %e,
                            "failed to mark earnings as published"
                        );
                    } else {
                        info!(
                            ticker = %ticker,
                            fiscal_year = record.fiscal_year,
                            fiscal_quarter = record.fiscal_quarter,
                            "published EarningsEvent"
                        );
                    }

                    let eps = record.eps_actual.unwrap_or(0.0);
                    let rev = record.revenue_actual.unwrap_or(0.0);
                    let description = if eps != 0.0 || rev != 0.0 {
                        let rev_m = rev / 1_000_000.0;
                        format!(
                            "Q{} {}: EPS ${:.2}, Rev ${:.0}M",
                            record.fiscal_quarter, record.fiscal_year, eps, rev_m
                        )
                    } else {
                        String::new()
                    };
                    let special = SpecialEvent {
                        ticker: ticker.to_string(),
                        event_type: "earnings".to_string(),
                        occurred_at_unix_secs: announced_at_unix_secs,
                        description,
                        ..Default::default()
                    };
                    if let Err(e) = producer
                        .publish_special_event(&args.special_events_topic, &special)
                        .await
                    {
                        error!(ticker = %ticker, error = %e, "special event publish failed");
                    }
                }
                Err(e) => {
                    error!(ticker = %ticker, fiscal_year = record.fiscal_year, fiscal_quarter = record.fiscal_quarter, error = %e, "failed to publish EarningsEvent");
                }
            }
        }
    }
    Ok(())
}

/// Converts a [`chrono::NaiveDate`] to a UTC midnight Unix timestamp in seconds.
fn naive_date_to_unix_secs(date: chrono::NaiveDate) -> i64 {
    let midnight = NaiveDateTime::new(date, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    Utc.from_utc_datetime(&midnight).timestamp()
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
            "earnings",
            "--database-url",
            "not-a-url",
            "--kafka-brokers",
            "none:1",
            "--polygon-api-key",
            "k",
        ]);
        assert!(run_job(args).await.is_err());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/metrics/job/earnings");
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains("nexus_job_last_run_success{job=\"earnings\"} 0"));
    }
}
