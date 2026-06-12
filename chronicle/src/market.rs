mod db;
mod kafka;

use alpha::{BarProvider, PolygonBarProvider};
use clap::Parser;
use kafka::ChronicleProducer;
use model::asset::Asset;
use sqlx::postgres::PgPoolOptions;
use time::macros::format_description;
use time::Date;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(about = "Fetch EOD bars from Polygon and publish as MarketEvent to Kafka")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "POLYGON_API_KEY")]
    api_key: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    brokers: String,

    #[arg(long, env = "KAFKA_TOPIC", default_value = "market.bars")]
    topic: String,

    #[arg(long)]
    from: String,

    #[arg(long)]
    to: String,
}

fn parse_date(s: &str) -> Date {
    let fmt = format_description!("[year]-[month]-[day]");
    Date::parse(s, fmt).unwrap_or_else(|_| panic!("invalid date: {s}"))
}

#[tokio::main]
async fn main() {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let from = parse_date(&args.from);
    let to = parse_date(&args.to);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&args.database_url)
        .await
        .expect("failed to connect to postgres");

    let tickers = db::load_tickers(&pool)
        .await
        .expect("failed to load tickers from companies table");

    if tickers.is_empty() {
        warn!("no tickers registered in the companies table — register tickers via the company.tickers Kafka topic first");
        return;
    }

    info!(count = tickers.len(), "loaded tickers from companies table");

    let polygon = PolygonBarProvider::new(&args.api_key);
    let producer = ChronicleProducer::new(&args.brokers).expect("failed to connect to Kafka");

    for ticker in &tickers {
        let asset = Asset::new(ticker);
        info!("fetching bars for {ticker} from {from} to {to}");

        let bars = match polygon.bars(&asset, from, to).await {
            Ok(b) => b,
            Err(e) => {
                error!("failed to fetch {ticker}: {e}");
                continue;
            }
        };

        info!("publishing {} bars for {ticker}", bars.len());
        for bar in &bars {
            if let Err(e) = producer.publish_bar(&args.topic, bar).await {
                error!("publish failed for {ticker}: {e}");
            }
        }
        info!("done with {ticker}");
    }
}
