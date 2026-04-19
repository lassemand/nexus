mod db;
mod strategy;

use alpha::YahooPriceProvider;
use db::insert_trade_result;
use model::generated::EarningsEvent;
use prost::Message;
use rdkafka::{
    consumer::{CommitMode, Consumer, StreamConsumer},
    ClientConfig, Message as KafkaMessage,
};
use sqlx::postgres::PgPoolOptions;
use tracing::{error, info, warn};

const BROKERS: &str = "localhost:19092";
const EARNINGS_TOPIC: &str = "earnings.calendar";
const DATABASE_URL: &str = "postgres://nexus:nexus@localhost:5432/nexus";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(DATABASE_URL)
        .await
        .expect("failed to connect to postgres");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations failed");

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", BROKERS)
        .set("group.id", "backtest")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("failed to create kafka consumer");

    consumer
        .subscribe(&[EARNINGS_TOPIC])
        .expect("failed to subscribe");

    let pricer = YahooPriceProvider::new();

    info!("backtest consumer started, waiting for earnings events");

    loop {
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
