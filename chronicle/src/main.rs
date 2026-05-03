mod db;
mod edgar;
mod kafka;
mod parse;

use clap::Parser;
use edgar::{EdgarClient, Filing};
use kafka::ChronicleProducer;
use model::generated::EarningsEvent;
use sqlx::postgres::PgPoolOptions;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(about = "Ingest SEC EDGAR earnings filings into Kafka")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS", default_value = "localhost:19092")]
    brokers: String,

    #[arg(long, env = "KAFKA_TOPIC", default_value = "earnings.calendar")]
    topic: String,

    #[arg(
        long,
        env = "TICKERS",
        value_delimiter = ',',
        default_value = "AAPL,MSFT,GOOGL"
    )]
    tickers: Vec<String>,
}

#[tokio::main]
async fn main() {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&args.database_url)
        .await
        .expect("failed to connect to postgres");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations failed");

    let edgar = EdgarClient::new().expect("failed to create EDGAR client");
    let producer = ChronicleProducer::new(&args.brokers).expect("failed to connect to Kafka");

    for ticker in &args.tickers {
        info!("fetching CIK for {ticker}");
        let cik = match edgar.cik_for_ticker(ticker).await {
            Ok(cik) => cik,
            Err(e) => {
                error!("cik lookup failed for {ticker}: {e}");
                continue;
            }
        };

        info!("fetching 8-K filings for {ticker} (CIK {cik})");
        let filings = match edgar.earnings_filings(cik).await {
            Ok(f) => f,
            Err(e) => {
                error!("filings fetch failed for {ticker}: {e}");
                continue;
            }
        };

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
                if let Err(e) = db::mark_published(&pool, &accession_number, ticker).await {
                    error!("failed to mark {accession_number} as published: {e}");
                } else {
                    info!("published {ticker} filing {accession_number} filed {filed_at} period {report_date}");
                }
            }
        }
    }
}
