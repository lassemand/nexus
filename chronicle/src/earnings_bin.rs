mod db;
mod earnings_db;
mod kafka;

use alpha::{EarningsProvider, PolygonEarningsError, PolygonEarningsProvider};
use chrono::{Duration, NaiveDateTime, NaiveTime, TimeZone, Utc};
use clap::Parser;
use earnings_db::{load_unpublished_earnings, mark_earnings_published, upsert_earnings_date};
use kafka::ChronicleProducer;
use model::{
    asset::Asset,
    generated::{EarningsEvent, SpecialEvent},
};
use sqlx::postgres::PgPoolOptions;
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

    let tickers = db::load_tickers(&pool)
        .await
        .expect("failed to load tickers from companies table");

    if tickers.is_empty() {
        warn!("no tickers registered in the companies table — register tickers first");
        return;
    }

    info!(count = tickers.len(), "loaded tickers from companies table");

    let polygon = PolygonEarningsProvider::new(&args.polygon_api_key);
    let producer = ChronicleProducer::new(&args.kafka_brokers).expect("failed to connect to Kafka");

    let from_date = Utc::now().date_naive() - Duration::days(365 * args.lookback_years as i64);

    for (i, ticker) in tickers.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(12)).await;
        }

        let asset = Asset::new(ticker);

        info!(ticker = %ticker, from = %from_date, "fetching quarterly earnings");

        let earnings = match polygon.quarterly_earnings(&asset, from_date).await {
            Ok(e) => e,
            Err(PolygonEarningsError::RateLimited) => {
                warn!(ticker = %ticker, "rate limited by Polygon, sleeping 12s and retrying");
                tokio::time::sleep(std::time::Duration::from_secs(12)).await;
                match polygon.quarterly_earnings(&asset, from_date).await {
                    Ok(e) => e,
                    Err(e) => {
                        error!(ticker = %ticker, error = %e, "retry failed after rate limit, skipping ticker");
                        continue;
                    }
                }
            }
            Err(e) => {
                error!(ticker = %ticker, error = %e, "failed to fetch earnings, skipping ticker");
                continue;
            }
        };

        info!(ticker = %ticker, count = earnings.len(), "fetched quarterly earnings");

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
}

/// Converts a [`chrono::NaiveDate`] to a UTC midnight Unix timestamp in seconds.
fn naive_date_to_unix_secs(date: chrono::NaiveDate) -> i64 {
    let midnight = NaiveDateTime::new(date, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    Utc.from_utc_datetime(&midnight).timestamp()
}
