mod db;
mod strategy;

use alpha::YahooPriceProvider;
use clap::Parser;
use db::insert_trade_result;
use model::generated::EarningsEvent;
use prost::Message;
use rdkafka::{
    consumer::{CommitMode, Consumer, StreamConsumer},
    ClientConfig, Message as KafkaMessage,
};
use sqlx::postgres::PgPoolOptions;
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
