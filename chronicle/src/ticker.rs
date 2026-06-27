/// Long-running Kafka consumer that syncs `company.tickers` registrations
/// into the `chronicle.companies` table.
///
/// This keeps chronicle's view of registered tickers in sync with the rest
/// of the pipeline. Without it, the CronJobs (chronicle, market, earnings,
/// filings) would never know which tickers to fetch data for.
use clap::Parser;
use model::generated::TickerRegistration;
use prost::Message;
use rdkafka::{
    consumer::{CommitMode, Consumer, StreamConsumer},
    ClientConfig, Message as KafkaMessage,
};
use sqlx::postgres::PgPoolOptions;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(about = "Consume company.tickers and persist registrations to chronicle DB")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    brokers: String,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
    topic: String,

    #[arg(long, env = "KAFKA_GROUP_ID", default_value = "chronicle-ticker")]
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
        .expect("failed to subscribe to company.tickers");

    info!(
        brokers = %args.brokers,
        topic = %args.topic,
        group_id = %args.group_id,
        "ticker consumer started"
    );

    loop {
        match consumer.recv().await {
            Err(e) => error!("kafka recv error: {e}"),
            Ok(msg) => {
                let Some(payload) = msg.payload() else {
                    warn!("empty payload, skipping");
                    consumer.commit_message(&msg, CommitMode::Async).ok();
                    continue;
                };

                let registration = match TickerRegistration::decode(payload) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("malformed TickerRegistration protobuf, skipping: {e}");
                        consumer.commit_message(&msg, CommitMode::Async).ok();
                        continue;
                    }
                };

                let ticker = registration.ticker.to_uppercase();

                match sqlx::query(
                    "INSERT INTO companies (ticker) VALUES ($1) ON CONFLICT (ticker) DO NOTHING",
                )
                .bind(&ticker)
                .execute(&pool)
                .await
                {
                    Ok(_) => info!(ticker = %ticker, "ticker registered in chronicle"),
                    Err(e) => error!(ticker = %ticker, error = %e, "failed to persist ticker"),
                }

                consumer.commit_message(&msg, CommitMode::Async).ok();
            }
        }
    }
}
