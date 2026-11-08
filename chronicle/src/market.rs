mod kafka;

use alpha::polygon::PolygonError;
use alpha::{BarProvider, PolygonBarProvider};
use chronicle_lib::job_metrics;
use clap::Parser;
use kafka::{load_tickers, ChronicleProducer};
use model::asset::Asset;
use std::process::ExitCode;
use time::macros::format_description;
use time::Date;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(about = "Fetch EOD bars from Polygon and publish as MarketEvent to Kafka")]
struct Args {
    #[arg(long, env = "POLYGON_API_KEY")]
    api_key: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    brokers: String,

    #[arg(long, env = "KAFKA_TOPIC", default_value = "market.bars")]
    topic: String,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
    tickers_topic: String,

    #[arg(long)]
    from: String,

    #[arg(long)]
    to: String,
}

/// Job name in the Pushgateway and the prefix of the run metrics.
const JOB: &str = "market";

fn parse_date(s: &str) -> anyhow::Result<Date> {
    let fmt = format_description!("[year]-[month]-[day]");
    Date::parse(s, fmt).map_err(|e| anyhow::anyhow!("invalid date {s}: {e}"))
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();
    match run_job(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %format!("{e:#}"), "market failed");
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
    let from = parse_date(&args.from)?;
    let to = parse_date(&args.to)?;

    let tickers = load_tickers(&args.brokers, &args.tickers_topic);

    if tickers.is_empty() {
        warn!("no tickers registered — publish a TickerRegistration to the company.tickers topic first");
        return Ok(());
    }

    info!(count = tickers.len(), "loaded tickers from kafka");

    let polygon = PolygonBarProvider::new(&args.api_key);
    let producer = ChronicleProducer::new(&args.brokers)?;

    for ticker in &tickers {
        let asset = Asset::new(ticker);
        info!("fetching bars for {ticker} from {from} to {to}");

        let bars = match polygon.bars(&asset, from, to).await {
            Ok(b) => b,
            Err(e) => {
                error!("failed to fetch {ticker}: {e}");
                job_metrics::api_error(
                    JOB,
                    match &e {
                        PolygonError::Http(e) => job_metrics::class_of_reqwest(e),
                        PolygonError::Api { .. } => "other",
                    },
                );
                continue;
            }
        };
        job_metrics::items_fetched(JOB, bars.len());

        info!("publishing {} bars for {ticker}", bars.len());
        for bar in &bars {
            match producer.publish_bar(&args.topic, bar).await {
                Ok(()) => job_metrics::events_published(JOB, 1),
                Err(e) => error!("publish failed for {ticker}: {e}"),
            }
        }
        info!("done with {ticker}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A forced error (bad date) still pushes the run, as a failure.
    #[tokio::test]
    async fn a_failed_run_still_pushes_its_metrics() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        std::env::set_var("PUSHGATEWAY_URL", server.uri());
        let args = Args::parse_from([
            "market",
            "--api-key",
            "k",
            "--brokers",
            "none:1",
            "--from",
            "bad",
            "--to",
            "2024-01-02",
        ]);
        assert!(run_job(args).await.is_err());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/metrics/job/market");
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains("nexus_job_last_run_success{job=\"market\"} 0"));
    }
}
