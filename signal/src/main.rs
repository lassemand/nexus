mod db;
mod features;
mod strategy;

use alpha::{CompanyProvider, PolygonSectorProvider, YahooPriceProvider};
use chrono::DateTime;
use clap::Parser;
use db::{
    fetch_bars_after, fetch_bars_before, fetch_unlabeled_events_needing_bar, insert_event_signal,
    insert_trade_result, is_registered, label_event_truth, upsert_bar, upsert_company,
    upsert_insider_filing, upsert_special_event,
};
use features::{compute_post_event_ar, compute_pre_event_features};
use model::{
    asset::{mic, Asset},
    generated::{
        EarningsEvent, MarketEvent, SpecialEvent, TickerRegistration, UnifiedInsiderTransaction,
    },
};
use prost::Message;
use rdkafka::{
    consumer::{CommitMode, Consumer, StreamConsumer},
    ClientConfig, Message as KafkaMessage,
};
use sqlx::{postgres::PgPoolOptions, PgPool};
use tracing::{debug, error, info, warn};

#[derive(Parser)]
#[command(about = "Backtest earnings strategy against historical data")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long, env = "KAFKA_BROKERS")]
    brokers: String,

    #[arg(long, env = "KAFKA_TOPIC", default_value = "earnings.calendar")]
    topic: String,

    #[arg(long, env = "KAFKA_GROUP_ID", default_value = "backtest")]
    group_id: String,

    #[arg(long, env = "POLYGON_API_KEY")]
    polygon_api_key: String,

    #[arg(long, env = "KAFKA_TICKERS_TOPIC", default_value = "company.tickers")]
    tickers_topic: String,

    #[arg(long, env = "SPECIAL_EVENTS_TOPIC", default_value = "special.events")]
    special_events_topic: String,

    #[arg(long, env = "MARKET_BARS_TOPIC", default_value = "market.bars")]
    market_bars_topic: String,

    #[arg(
        long,
        env = "INSIDER_TRANSACTIONS_TOPIC",
        default_value = "insider.transactions"
    )]
    insider_transactions_topic: String,
}

/// Handles a raw `TickerRegistration` protobuf payload from the
/// `company.tickers` Kafka topic.
///
/// - Decodes the message; logs a warning and returns on malformed bytes.
/// - Normalises the ticker to uppercase.
/// - Fetches the GICS sector from Polygon (SIC-based); logs an error and
///   returns on API failure, but the caller should still commit the offset.
/// - On success, persists the mapping to Postgres.
async fn handle_ticker_registration(
    payload: &[u8],
    pool: &PgPool,
    sector_provider: &PolygonSectorProvider,
) {
    let msg = match TickerRegistration::decode(payload) {
        Ok(m) => m,
        Err(e) => {
            warn!("malformed TickerRegistration protobuf, skipping: {e}");
            return;
        }
    };

    let ticker = msg.ticker.to_uppercase();
    // Resolve exchange_mic: use the value from the proto if present, fall back to XNAS.
    let exchange_mic = if msg.exchange_mic.is_empty() {
        mic::XNAS.to_string()
    } else {
        msg.exchange_mic.clone()
    };
    // Resolve currency: use explicit proto value if present, else derive from MIC.
    let currency = if msg.currency.is_empty() {
        mic::currency(&exchange_mic).to_string()
    } else {
        msg.currency.clone()
    };

    let asset = Asset::with_mic(&ticker, &exchange_mic);

    let sector = match sector_provider.sector(&asset).await {
        Ok(s) => s,
        Err(e) => {
            error!(ticker = %ticker, exchange_mic = %exchange_mic, error = %e,
                   "Polygon sector lookup failed, skipping ticker");
            return;
        }
    };

    if let Err(e) = upsert_company(pool, &ticker, &exchange_mic, &currency, sector).await {
        error!(ticker = %ticker, error = %e, "failed to persist company to DB");
        return;
    }

    info!(ticker = %ticker, exchange_mic = %exchange_mic, currency = %currency,
          "ticker registered and sector persisted");
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

    let sector_provider = PolygonSectorProvider::new(&args.polygon_api_key);

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &args.brokers)
        .set("group.id", &args.group_id)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()
        .expect("failed to create kafka consumer");

    consumer
        .subscribe(&[
            args.tickers_topic.as_str(),
            args.topic.as_str(),
            args.special_events_topic.as_str(),
            args.market_bars_topic.as_str(),
            args.insider_transactions_topic.as_str(),
        ])
        .expect("failed to subscribe");

    let pricer = YahooPriceProvider::new();

    info!(
        brokers = %args.brokers,
        earnings_topic = %args.topic,
        tickers_topic = %args.tickers_topic,
        special_events_topic = %args.special_events_topic,
        group_id = %args.group_id,
        "signal consumer started"
    );

    loop {
        match consumer.recv().await {
            Err(e) => error!("kafka recv error: {e}"),
            Ok(msg) => {
                let topic = msg.topic();
                let Some(payload) = msg.payload() else {
                    warn!(topic, "empty payload, skipping");
                    consumer.commit_message(&msg, CommitMode::Async).ok();
                    continue;
                };

                if topic == args.tickers_topic {
                    handle_ticker_registration(payload, &pool, &sector_provider).await;
                } else if topic == args.topic {
                    let event = match EarningsEvent::decode(payload) {
                        Ok(e) => e,
                        Err(e) => {
                            error!("protobuf decode failed: {e}");
                            consumer.commit_message(&msg, CommitMode::Async).ok();
                            continue;
                        }
                    };

                    let ticker = event.ticker.to_uppercase();
                    match is_registered(&pool, &ticker).await {
                        Ok(false) => {
                            debug!(ticker = %ticker, "skipping unregistered ticker");
                            consumer.commit_message(&msg, CommitMode::Async).ok();
                            continue;
                        }
                        Err(e) => {
                            error!(ticker = %ticker, error = %e, "DB registration check failed, skipping");
                            consumer.commit_message(&msg, CommitMode::Async).ok();
                            continue;
                        }
                        Ok(true) => {}
                    }

                    info!("evaluating earnings strategy for {ticker}");

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

                    // Compute and store pre-event features regardless of strategy outcome.
                    let event_date =
                        chrono::DateTime::from_timestamp(event.announced_at_unix_secs, 0)
                            .map(|dt| dt.date_naive());

                    if let Some(event_date) = event_date {
                        match fetch_bars_before(&pool, &ticker, event_date, 80).await {
                            Ok(bars) => {
                                let features = compute_pre_event_features(&bars);
                                if let Err(e) = insert_event_signal(
                                    &pool, &ticker, "earnings", event_date, &features,
                                )
                                .await
                                {
                                    error!(ticker = %ticker, error = %e, "failed to insert event signal");
                                }
                            }
                            Err(e) => {
                                error!(ticker = %ticker, error = %e, "failed to fetch bars for feature computation");
                            }
                        }
                    }
                } else if topic == args.special_events_topic {
                    let event = match SpecialEvent::decode(payload) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!("malformed SpecialEvent protobuf, skipping: {e}");
                            consumer.commit_message(&msg, CommitMode::Async).ok();
                            continue;
                        }
                    };

                    let Some(occurred_at) =
                        DateTime::from_timestamp(event.occurred_at_unix_secs, 0)
                    else {
                        warn!(
                            ticker = %event.ticker,
                            secs = event.occurred_at_unix_secs,
                            "invalid occurred_at timestamp, skipping"
                        );
                        consumer.commit_message(&msg, CommitMode::Async).ok();
                        continue;
                    };

                    let description = if event.description.is_empty() {
                        None
                    } else {
                        Some(event.description.as_str())
                    };

                    if let Err(e) = upsert_special_event(
                        &pool,
                        &event.ticker,
                        &event.event_type,
                        occurred_at,
                        description,
                    )
                    .await
                    {
                        warn!(ticker = %event.ticker, error = %e, "failed to persist special event");
                    } else {
                        info!(
                            ticker = %event.ticker,
                            event_type = %event.event_type,
                            occurred_at = %occurred_at,
                            "special event persisted"
                        );
                    }
                } else if topic == args.market_bars_topic {
                    let event = match MarketEvent::decode(payload) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!("malformed MarketEvent protobuf, skipping: {e}");
                            consumer.commit_message(&msg, CommitMode::Async).ok();
                            continue;
                        }
                    };

                    let Some(date) = DateTime::from_timestamp(event.timestamp_unix_secs, 0)
                        .map(|dt| dt.date_naive())
                    else {
                        warn!(
                            ticker = %event.ticker,
                            secs = event.timestamp_unix_secs,
                            "invalid bar timestamp, skipping"
                        );
                        consumer.commit_message(&msg, CommitMode::Async).ok();
                        continue;
                    };

                    if let Err(e) = upsert_bar(
                        &pool,
                        &event.ticker,
                        date,
                        event.open,
                        event.high,
                        event.low,
                        event.close,
                        event.volume as i64,
                    )
                    .await
                    {
                        warn!(ticker = %event.ticker, date = %date, error = %e, "failed to persist bar");
                    } else {
                        info!(ticker = %event.ticker, date = %date, "bar persisted");
                    }

                    // Lazy truth labeling: check if this bar completes the
                    // post-event window for any unlabeled event_signals rows.
                    match fetch_unlabeled_events_needing_bar(&pool, &event.ticker, date).await {
                        Err(e) => {
                            warn!(ticker = %event.ticker, error = %e, "failed to fetch unlabeled events")
                        }
                        Ok(unlabeled_events) => {
                            for unlabeled in unlabeled_events {
                                // Fetch baseline + pre-event bars, then post-event bars separately
                                // so we don't need a date field on Bar to split them.
                                let pre_bars = match fetch_bars_before(
                                    &pool,
                                    &event.ticker,
                                    unlabeled.event_date,
                                    80,
                                )
                                .await
                                {
                                    Ok(b) => b,
                                    Err(e) => {
                                        warn!(error = %e, "failed to fetch pre-event bars for truth labeling");
                                        continue;
                                    }
                                };
                                let post_bars = match fetch_bars_after(
                                    &pool,
                                    &event.ticker,
                                    unlabeled.event_date,
                                    5,
                                )
                                .await
                                {
                                    Ok(b) => b,
                                    Err(e) => {
                                        warn!(error = %e, "failed to fetch post-event bars for truth labeling");
                                        continue;
                                    }
                                };
                                let event_idx = pre_bars.len();
                                let mut combined = pre_bars;
                                combined.extend(post_bars);
                                if let Some(ar) = compute_post_event_ar(&combined, event_idx) {
                                    if let Err(e) =
                                        label_event_truth(&pool, unlabeled.id, ar.ar_1d, ar.ar_5d)
                                            .await
                                    {
                                        warn!(id = unlabeled.id, error = %e, "failed to label event truth");
                                    } else {
                                        info!(
                                            id = unlabeled.id,
                                            event_date = %unlabeled.event_date,
                                            ticker = %event.ticker,
                                            ar_1d = ar.ar_1d,
                                            ar_5d = ar.ar_5d,
                                            "event truth labeled"
                                        );
                                    }
                                }
                            }
                        }
                    }
                } else if topic == args.insider_transactions_topic {
                    let txn = match UnifiedInsiderTransaction::decode(payload) {
                        Ok(t) => t,
                        Err(e) => {
                            warn!("malformed UnifiedInsiderTransaction protobuf, skipping: {e}");
                            consumer.commit_message(&msg, CommitMode::Async).ok();
                            continue;
                        }
                    };

                    if let Err(e) = upsert_insider_filing(&pool, &txn).await {
                        warn!(ticker = %txn.ticker, error = %e, "failed to persist insider transaction");
                    } else {
                        info!(
                            ticker = %txn.ticker,
                            filer = %txn.person_name,
                            role = %txn.person_role,
                            source = txn.source_registry,
                            txn_date = %txn.transaction_date,
                            volume = txn.volume,
                            price = txn.price_per_unit,
                            "insider transaction persisted"
                        );
                    }
                } else {
                    warn!(topic, "message from unknown topic, skipping");
                }

                consumer.commit_message(&msg, CommitMode::Async).ok();
            }
        }
    }
}
