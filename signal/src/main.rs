mod db;
mod strategy;

use alpha::{CompanyProvider, PolygonSectorProvider, YahooPriceProvider};
use clap::Parser;
use db::{insert_trade_result, load_companies, upsert_company};
use model::{
    asset::Asset, generated::EarningsEvent, generated::TickerRegistration, sector::Sector,
};
use prost::Message;
use rdkafka::{
    consumer::{CommitMode, Consumer, StreamConsumer},
    ClientConfig, Message as KafkaMessage,
};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::collections::HashMap;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(about = "Backtest earnings strategy against historical data")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS", default_value = "localhost:19092")]
    brokers: String,

    #[arg(long, env = "KAFKA_TOPIC", default_value = "earnings.calendar")]
    topic: String,

    #[arg(long, env = "KAFKA_GROUP_ID", default_value = "backtest")]
    group_id: String,

    #[arg(long, env = "POLYGON_API_KEY")]
    polygon_api_key: String,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
    tickers_topic: String,
}

/// Handles a raw `TickerRegistration` protobuf payload from the
/// `company.tickers` Kafka topic.
///
/// - Decodes the message; logs a warning and returns on malformed bytes.
/// - Normalises the ticker to uppercase.
/// - Fetches the GICS sector from Polygon (SIC-based); logs an error and
///   returns on API failure, but the caller should still commit the offset.
/// - On success, persists the mapping to Postgres and updates `cache`.
// Not yet wired into the Kafka loop (that is NEX-9); #[allow] will be removed then.
#[allow(dead_code)]
async fn handle_ticker_registration(
    payload: &[u8],
    pool: &PgPool,
    sector_provider: &PolygonSectorProvider,
    cache: &mut HashMap<String, Sector>,
) {
    let msg = match TickerRegistration::decode(payload) {
        Ok(m) => m,
        Err(e) => {
            warn!("malformed TickerRegistration protobuf, skipping: {e}");
            return;
        }
    };

    let ticker = msg.ticker.to_uppercase();
    let asset = Asset::new(&ticker);

    let sector = match sector_provider.sector(&asset).await {
        Ok(s) => s,
        Err(e) => {
            error!(ticker = %ticker, error = %e, "Polygon sector lookup failed, skipping ticker");
            return;
        }
    };

    if let Err(e) = upsert_company(pool, &ticker, sector.clone()).await {
        error!(ticker = %ticker, error = %e, "failed to persist company sector to DB");
        return;
    }

    cache.insert(ticker.clone(), sector);
    info!(ticker = %ticker, "ticker registered and sector persisted");
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

    // Load the persisted ticker→sector map so the in-memory cache survives
    // process restarts without replaying the full company.tickers topic.
    // `mut` needed once handle_ticker_registration is wired in (NEX-9).
    #[allow(unused_mut)]
    let mut companies: HashMap<String, Sector> = load_companies(&pool)
        .await
        .expect("failed to load companies from DB");

    let _sector_provider = PolygonSectorProvider::new(&args.polygon_api_key);

    info!(
        tickers_loaded = companies.len(),
        tickers_topic = %args.tickers_topic,
        "companies cache initialised"
    );

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &args.brokers)
        .set("group.id", &args.group_id)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()
        .expect("failed to create kafka consumer");

    consumer
        .subscribe(&[args.topic.as_str()])
        .expect("failed to subscribe");

    let pricer = YahooPriceProvider::new();

    info!(
        brokers = %args.brokers,
        topic = %args.topic,
        group_id = %args.group_id,
        "backtest consumer started"
    );

    loop {
        info!("polling for next message...");
        match consumer.recv().await {
            Err(e) => error!("kafka recv error: {e}"),
            Ok(msg) => {
                let Some(payload) = msg.payload() else {
                    warn!("empty payload, skipping");
                    consumer.commit_message(&msg, CommitMode::Async).ok();
                    continue;
                };

                let event = match EarningsEvent::decode(payload) {
                    Ok(e) => e,
                    Err(e) => {
                        error!("protobuf decode failed: {e}");
                        consumer.commit_message(&msg, CommitMode::Async).ok();
                        continue;
                    }
                };

                // Gate on the in-memory sector cache; tickers not yet
                // registered via company.tickers are silently skipped here
                // until NEX-9 integrates the full dual-topic consumer.
                if !companies.contains_key(&event.ticker) {
                    warn!(ticker = %event.ticker, "ticker not in companies cache, skipping");
                    consumer.commit_message(&msg, CommitMode::Async).ok();
                    continue;
                }

                info!("evaluating earnings strategy for {}", event.ticker);

                match strategy::evaluate(&pricer, &event).await {
                    Ok(result) => {
                        info!(
                            ticker = %result.ticker,
                            buy_date = %result.buy_date,
                            earnings_date = %result.earnings_date,
                            buy_price = result.buy_price,
                            sell_price = result.sell_price,
                            pnl = result.pnl,
                            pnl_pct = result.pnl_pct,
                            "trade simulated"
                        );

                        if let Err(e) = insert_trade_result(&pool, &result).await {
                            error!("db insert failed for {}: {e}", result.ticker);
                        }
                    }
                    Err(e) => error!("strategy evaluation failed for {}: {e}", event.ticker),
                }

                consumer.commit_message(&msg, CommitMode::Async).ok();
            }
        }
    }
}
