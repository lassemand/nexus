mod db;
pub mod earnings_db;
mod edgar;
mod kafka;
mod parse;

use anyhow::Context;
use chronicle_lib::job_metrics;
use clap::Parser;
use edgar::{EdgarClient, Filing};
use kafka::{load_tickers, ChronicleProducer};
use model::generated::{EarningsEvent, SpecialEvent};
use sqlx::postgres::PgPoolOptions;
use std::process::ExitCode;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(about = "Ingest SEC EDGAR earnings filings into Kafka")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    brokers: String,

    #[arg(long, env = "KAFKA_TOPIC", default_value = "earnings.calendar")]
    topic: String,

    #[arg(long, env = "KAFKA_GROUP_ID", default_value = "chronicle")]
    group_id: String,

    #[arg(long, env = "SPECIAL_EVENTS_TOPIC", default_value = "special.events")]
    special_events_topic: String,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
    tickers_topic: String,
}

/// Job name in the Pushgateway and the prefix of the run metrics. Units: `items`
/// are 8-K earnings filings found on EDGAR, `events` are `EarningsEvent`s published.
const JOB: &str = "chronicle";

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();
    match run_job(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %format!("{e:#}"), "chronicle failed");
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

    let tickers = load_tickers(&args.brokers, &args.tickers_topic);

    if tickers.is_empty() {
        warn!("no tickers registered — publish a TickerRegistration to the company.tickers topic first");
        return Ok(());
    }

    info!(count = tickers.len(), "loaded tickers from kafka");

    let edgar = EdgarClient::new().context("failed to create EDGAR client")?;
    let producer = ChronicleProducer::new(&args.brokers).context("failed to connect to Kafka")?;

    for ticker in &tickers {
        info!("fetching CIK for {ticker}");
        let cik = match edgar.cik_for_ticker(ticker).await {
            Ok(cik) => cik,
            Err(e) => {
                error!("cik lookup failed for {ticker}: {e}");
                job_metrics::api_error(JOB, edgar_class(&e));
                continue;
            }
        };

        info!("fetching 8-K filings for {ticker} (CIK {cik})");
        let filings = match edgar.earnings_filings(cik).await {
            Ok(f) => f,
            Err(e) => {
                error!("filings fetch failed for {ticker}: {e}");
                job_metrics::api_error(JOB, edgar_class(&e));
                continue;
            }
        };
        job_metrics::items_fetched(JOB, filings.len());

        for Filing {
            filed_at,
            report_date,
            accession_number,
            ..
        } in filings
        {
            match db::is_published(&pool, &accession_number).await {
                Ok(true) => {
                    warn!("skipping already published filing {accession_number}");
                    continue;
                }
                Err(e) => {
                    error!("db check failed for {accession_number}: {e}");
                    continue;
                }
                Ok(false) => {}
            }

            let event = EarningsEvent {
                ticker: ticker.to_string(),
                announced_at_unix_secs: parse::filed_at_to_unix(&filed_at).unwrap_or(0),
                report_period_unix_secs: parse::filed_at_to_unix(&report_date).unwrap_or(0),
                fiscal_quarter: parse::fiscal_quarter(&report_date).unwrap_or(0),
                fiscal_year: parse::fiscal_year(&report_date).unwrap_or(0),
                eps_actual: 0.0,
                eps_estimate: 0.0,
                revenue_actual: 0.0,
                revenue_estimate: 0.0,
                cik,
                filing_url: parse::filing_url(cik, &accession_number),
            };

            if let Err(e) = producer.publish(&args.topic, ticker, &event).await {
                error!("publish failed for {ticker}: {e}");
            } else {
                job_metrics::events_published(JOB, 1);
                if let Err(e) = db::mark_published(&pool, &accession_number, ticker).await {
                    error!("failed to mark {accession_number} as published: {e}");
                } else {
                    info!("published {ticker} filing {accession_number} filed {filed_at} period {report_date}");
                }

                let special = SpecialEvent {
                    ticker: ticker.to_string(),
                    event_type: "earnings".to_string(),
                    occurred_at_unix_secs: event.announced_at_unix_secs,
                    description: String::new(),
                    ..Default::default()
                };
                if let Err(e) = producer
                    .publish_special_event(&args.special_events_topic, &special)
                    .await
                {
                    error!("special event publish failed for {ticker}: {e}");
                }
            }
        }
    }
    Ok(())
}

/// The `status_class` of an EDGAR failure.
fn edgar_class(e: &edgar::EdgarError) -> &'static str {
    match e {
        edgar::EdgarError::Http(e) => job_metrics::class_of_reqwest(e),
        edgar::EdgarError::CikNotFound(_) => "other",
    }
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
            "chronicle",
            "--database-url",
            "not-a-url",
            "--brokers",
            "none:1",
        ]);
        assert!(run_job(args).await.is_err());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/metrics/job/chronicle");
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains("nexus_job_last_run_success{job=\"chronicle\"} 0"));
    }
}
