mod analyst_store;
mod announcements;
mod calendar_sources;
mod daily_bars;
mod db;
#[cfg(test)]
mod e2e_announcements;
mod features;
mod insider_store;
mod reactions;
mod reports;
mod strategy;

use alpha::{CompanyProvider, PolygonSectorProvider, YahooPriceProvider};
use chrono::DateTime;
use clap::Parser;
use db::{
    delete_company, fetch_bars_after, fetch_bars_before, fetch_unlabeled_events_needing_bar,
    insert_event_signal, insert_trade_result, is_registered, label_event_truth, upsert_bar,
    upsert_company, upsert_insider_filing, upsert_special_event,
};
use features::{compute_post_event_ar, compute_pre_event_features};
use model::{
    asset::{mic, Asset},
    generated::{
        DailyBar, EarningsEvent, MarketEvent, SpecialEvent, TickerRegistration,
        UnifiedInsiderTransaction,
    },
    sector::Sector,
};
use prost::Message;
use rdkafka::{
    consumer::{CommitMode, Consumer, StreamConsumer},
    ClientConfig, Message as KafkaMessage,
};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::time::Duration;
use telemetry::trace::OpenTelemetrySpanExt;
use tracing::{debug, error, field::Empty, info, info_span, warn, Span};

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

    /// Official daily bars from saxo-stream (stored in `daily_bars`).
    #[arg(long, env = "DAILY_BARS_TOPIC", default_value = "market.daily_bars")]
    daily_bars_topic: String,

    #[arg(
        long,
        env = "INSIDER_TRANSACTIONS_TOPIC",
        default_value = "insider.transactions"
    )]
    insider_transactions_topic: String,

    /// The analyst's decisions (stored in `analyst_decisions`).
    #[arg(
        long,
        env = "ANALYST_DECISIONS_TOPIC",
        default_value = "analyst.decisions"
    )]
    analyst_decisions_topic: String,

    /// The analyst's simulated trades (stored in `simulated_trades`).
    #[arg(long, env = "ANALYST_TRADES_TOPIC", default_value = "analyst.trades")]
    analyst_trades_topic: String,
}

/// Handles a raw `TickerRegistration` protobuf payload from the
/// `company.tickers` Kafka topic.
///
/// - Decodes the message; logs a warning and returns on malformed bytes.
/// - Normalises the ticker to uppercase.
/// - An unregistration deletes the listing from `companies`, so its later
///   events are skipped; the data already stored for it is kept.
/// - Resolves the sector with [`resolve_sector`]; logs an error and returns
///   on API failure, but the caller should still commit the offset.
/// - On success, persists the mapping to Postgres.
///
/// Returns how the message ended, for the metrics.
async fn handle_ticker_registration<P: CompanyProvider>(
    payload: &[u8],
    pool: &PgPool,
    sector_provider: &P,
) -> &'static str {
    let msg = match TickerRegistration::decode(payload) {
        Ok(m) => m,
        Err(e) => {
            warn!("malformed TickerRegistration protobuf, skipping: {e}");
            return "skipped_malformed";
        }
    };

    let ticker = msg.ticker.to_uppercase();
    // Resolve exchange_mic: use the value from the proto if present, fall back to XNAS.
    let exchange_mic = if msg.exchange_mic.is_empty() {
        mic::XNAS.to_string()
    } else {
        msg.exchange_mic.clone()
    };
    if msg.unregister {
        return match delete_company(pool, &ticker, &exchange_mic).await {
            Ok(true) => {
                info!(ticker = %ticker, exchange_mic = %exchange_mic, "ticker unregistered");
                "stored"
            }
            Ok(false) => {
                info!(ticker = %ticker, exchange_mic = %exchange_mic,
                      "unregistration for a ticker that is not registered, nothing to do");
                "skipped_invalid"
            }
            Err(e) => {
                error!(ticker = %ticker, error = %e, "failed to delete company from DB");
                "db_error_committed"
            }
        };
    }
    // Resolve currency: use explicit proto value if present, else derive from MIC.
    let currency = if msg.currency.is_empty() {
        match mic::currency(&exchange_mic) {
            Some(c) => c.to_string(),
            None => {
                warn!(ticker = %ticker, exchange_mic = %exchange_mic,
                      "registration has no currency and MIC has no known currency, skipping");
                return "skipped_invalid";
            }
        }
    } else {
        msg.currency.clone()
    };

    let asset = Asset::with_mic(&ticker, &exchange_mic);

    let sector = match resolve_sector(sector_provider, &asset).await {
        Ok(s) => s,
        Err(e) => {
            error!(ticker = %ticker, exchange_mic = %exchange_mic, error = %e,
                   "Polygon sector lookup failed, skipping ticker");
            return "skipped_invalid";
        }
    };

    if let Err(e) = upsert_company(pool, &ticker, &exchange_mic, &currency, sector).await {
        error!(ticker = %ticker, error = %e, "failed to persist company to DB");
        return "db_error_committed";
    }

    info!(ticker = %ticker, exchange_mic = %exchange_mic, currency = %currency,
          "ticker registered and sector persisted");
    "stored"
}

/// Longest wait between retries of a failed Nordic report store.
pub(crate) const INITIAL_STORE_BACKOFF: Duration = Duration::from_secs(1);
pub(crate) const MAX_STORE_BACKOFF: Duration = Duration::from_secs(60);

/// Stores one Nordic report event, retrying transient database errors with
/// backoff while `set_blocked(true)` signals the stalled partition. Returns the
/// stored outcome, or `None` when the database rejected the row for good (it is
/// logged and counted in `signal_report_db_errors_total`, and the caller commits).
async fn store_report<F, Fut>(
    topic: &str,
    ticker: &str,
    event_type: &str,
    source_id: &str,
    op: F,
    initial: Duration,
    set_blocked: impl Fn(bool),
) -> Option<String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = sqlx::Result<String>>,
{
    let result = insider_store::store_with_retry_counted(
        "nordic report event",
        op,
        initial,
        MAX_STORE_BACKOFF,
        || {
            metrics::counter!("signal_messages_total", "topic" => topic.to_string(), "outcome" => "db_error_retried").increment(1);
            metrics::counter!("signal_store_retries_total", "topic" => topic.to_string()).increment(1);
            set_blocked(true);
        },
    )
    .await;
    set_blocked(false);
    match result {
        Ok(outcome) => {
            info!(ticker, event_type, source_id, %outcome, "Nordic report event stored");
            Some(outcome)
        }
        Err(e) => {
            error!(ticker, event_type, source_id, error = %e,
               "Nordic report event rejected by the database, skipped");
            metrics::counter!("signal_report_db_errors_total").increment(1);
            None
        }
    }
}

/// The `process special.events` consumer span of a message that carries a
/// `traceparent` header (nordic-live sets it on every live release), parented
/// on that context. `None` for every other message, so calendar snapshots and
/// poll-path events create no spans. Built from the already-decoded event and
/// the extracted header only: no await, no I/O.
fn release_span(msg: &impl KafkaMessage, event: &SpecialEvent) -> Option<Span> {
    let parent = telemetry::kafka::extract(msg)?;
    let key = msg
        .key()
        .map(|k| String::from_utf8_lossy(k).into_owned())
        .unwrap_or_default();
    let span = info_span!(
        "process special.events",
        otel.kind = "consumer",
        messaging.system = "kafka",
        messaging.destination.name = msg.topic(),
        messaging.kafka.message.key = key.as_str(),
        messaging.kafka.message.offset = msg.offset(),
        messaging.destination.partition.id = msg.partition(),
        nexus.ticker = event.ticker.as_str(),
        nexus.exchange_mic = event.exchange_mic.as_str(),
        mfn.news_id = event.source_id.as_str(),
        nexus.event_type = event.event_type.as_str(),
        otel.status_code = Empty,
        otel.status_description = Empty
    );
    span.set_parent(parent).ok()?;
    Some(span)
}

/// Returns the sector for `asset`, asking `provider` only for US listings.
///
/// Polygon covers US venues only: asking it about e.g. `GOMX` on `FNSE` is a
/// 404, which used to drop the whole registration. Other markets have no
/// sector source, so their sector is `None` (stored as NULL).
async fn resolve_sector<P: CompanyProvider>(
    provider: &P,
    asset: &Asset,
) -> Result<Option<Sector>, P::Error> {
    if !mic::is_us(&asset.exchange_mic) {
        info!(ticker = %asset.ticker, exchange_mic = %asset.exchange_mic,
              "no sector source for this exchange, sector left empty");
        return Ok(None);
    }
    provider.sector(asset).await.map(Some)
}

#[tokio::main]
async fn main() {
    dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok();
    let _tracing = telemetry::trace::init("signal").expect("failed to initialise tracing");

    let args = Args::parse();

    // Metrics must not take signal down: a port that cannot bind is only logged.
    if let Err(e) = telemetry::serve("signal", 9464).await {
        error!(error = %e, "metrics endpoint unavailable, continuing without it");
    }

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
            args.daily_bars_topic.as_str(),
            args.analyst_decisions_topic.as_str(),
            args.analyst_trades_topic.as_str(),
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

    // Catch-up for reports stored while prices were missing (NEX-163).
    if let Err(e) = reactions::refresh_all(&pool).await {
        warn!(error = %e, "report reaction catch-up sweep failed");
    }
    // Daily bars arrive in bursts (a backfill is thousands of messages): recompute
    // an instrument's report reactions once the burst is over.
    let mut reaction_queue =
        reactions::ReactionQueue::new(Duration::from_secs(2), Duration::from_secs(30));

    loop {
        if reaction_queue.is_due(std::time::Instant::now()) {
            reactions::flush(&pool, &mut reaction_queue).await;
        }
        let received = match tokio::time::timeout(Duration::from_secs(1), consumer.recv()).await {
            Ok(r) => r,
            Err(_) => continue, // idle: lets a pending flush happen
        };
        match received {
            Err(e) => error!("kafka recv error: {e}"),
            Ok(msg) => {
                let topic = msg.topic();
                let started = std::time::Instant::now();
                // Every exit of this block yields the message's outcome label.
                let outcome: &str = 'handled: {
                    let mut outcome = "stored";
                    let Some(payload) = msg.payload() else {
                        warn!(topic, "empty payload, skipping");
                        break 'handled "skipped_empty";
                    };

                    if topic == args.tickers_topic {
                        outcome =
                            handle_ticker_registration(payload, &pool, &sector_provider).await;
                    } else if topic == args.topic {
                        let event = match EarningsEvent::decode(payload) {
                            Ok(e) => e,
                            Err(e) => {
                                error!("protobuf decode failed: {e}");
                                break 'handled "skipped_malformed";
                            }
                        };

                        let ticker = event.ticker.to_uppercase();
                        match is_registered(&pool, &ticker).await {
                            Ok(false) => {
                                debug!(ticker = %ticker, "skipping unregistered ticker");
                                break 'handled "skipped_invalid";
                            }
                            Err(e) => {
                                error!(ticker = %ticker, error = %e, "DB registration check failed, skipping");
                                break 'handled "db_error_committed";
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
                                    outcome = "db_error_committed";
                                }
                            }
                            Err(e) => {
                                error!("strategy evaluation failed for {}: {e}", event.ticker)
                            }
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
                                        outcome = "db_error_committed";
                                    }
                                }
                                Err(e) => {
                                    error!(ticker = %ticker, error = %e, "failed to fetch bars for feature computation");
                                    outcome = "db_error_committed";
                                }
                            }
                        }
                    } else if topic == args.special_events_topic {
                        let event = match SpecialEvent::decode(payload) {
                            Ok(e) => e,
                            Err(e) => {
                                warn!("malformed SpecialEvent protobuf, skipping: {e}");
                                break 'handled "skipped_malformed";
                            }
                        };

                        let release_span = release_span(&msg, &event);
                        if event.source == reports::SOURCE_MFN {
                            let message = match reports::parse(&event) {
                                Ok(m) => m,
                                Err(e) => {
                                    warn!(ticker = %event.ticker, event_type = %event.event_type,
                                      error = %e, "invalid Nordic report event, skipping");
                                    break 'handled "skipped_invalid";
                                }
                            };
                            // Nordic reports exist only here and the producer won't
                            // resend them, so a transient DB error must not lose the
                            // event: retry with backoff and only then fall through to
                            // the commit. This blocks the consumer, which keeps each
                            // company's events in order.
                            // Only a transient error (see `insider_store::is_transient`)
                            // is retried. A row the schema rejects would block the
                            // partition forever and stall every later report behind
                            // it, so it is logged, counted and committed; the producer
                            // does not resend it, so it must be replayed from MFN.
                            let consumer_span = release_span.clone().unwrap_or_else(Span::none);
                            let stored = store_report(
                                topic,
                                &event.ticker,
                                &event.event_type,
                                &event.source_id,
                                || reports::handle_in(&pool, &message, &consumer_span),
                                INITIAL_STORE_BACKOFF,
                                |blocked| {
                                    metrics::gauge!("signal_store_blocked").set(if blocked {
                                        1.0
                                    } else {
                                        0.0
                                    })
                                },
                            )
                            .await;
                            if stored.is_none() {
                                outcome = "db_error_committed";
                            }
                        } else {
                            let Some(occurred_at) =
                                DateTime::from_timestamp(event.occurred_at_unix_secs, 0)
                            else {
                                warn!(
                                    ticker = %event.ticker,
                                    secs = event.occurred_at_unix_secs,
                                    "invalid occurred_at timestamp, skipping"
                                );
                                break 'handled "skipped_invalid";
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
                                outcome = "db_error_committed";
                            } else {
                                info!(
                                    ticker = %event.ticker,
                                    event_type = %event.event_type,
                                    occurred_at = %occurred_at,
                                    "special event persisted"
                                );
                            }
                        }
                    } else if topic == args.analyst_decisions_topic {
                        outcome = analyst_store::handle_decision(&pool, &msg).await;
                    } else if topic == args.analyst_trades_topic {
                        outcome = analyst_store::handle_trade(&pool, &msg).await;
                    } else if topic == args.market_bars_topic {
                        let event = match MarketEvent::decode(payload) {
                            Ok(e) => e,
                            Err(e) => {
                                warn!("malformed MarketEvent protobuf, skipping: {e}");
                                break 'handled "skipped_malformed";
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
                            break 'handled "skipped_invalid";
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
                            outcome = "db_error_committed";
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
                                        if let Err(e) = label_event_truth(
                                            &pool,
                                            unlabeled.id,
                                            ar.ar_1d,
                                            ar.ar_5d,
                                        )
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
                    } else if topic == args.daily_bars_topic {
                        let message = match DailyBar::decode(payload) {
                            Ok(m) => m,
                            Err(e) => {
                                warn!("malformed DailyBar protobuf, skipping: {e}");
                                break 'handled "skipped_malformed";
                            }
                        };
                        let bar = match daily_bars::parse(&message) {
                            Ok(b) => b,
                            Err(e) => {
                                warn!(ticker = %message.ticker, date = %message.date, error = %e,
                                  "invalid daily bar, skipping");
                                break 'handled "skipped_invalid";
                            }
                        };
                        // Same pattern as the other bar topics: a DB error is logged and the
                        // offset committed. saxo-stream republishes the last ten trading days
                        // every day, so a dropped bar heals itself.
                        if let Err(e) = daily_bars::store(&pool, &bar).await {
                            warn!(ticker = %bar.ticker, date = %bar.date, error = %e,
                              "failed to persist daily bar");
                            outcome = "db_error_committed";
                        } else {
                            debug!(ticker = %bar.ticker, mic = %bar.exchange_mic, date = %bar.date,
                               "daily bar persisted");
                            reaction_queue.add(
                                &bar.ticker,
                                &bar.exchange_mic,
                                bar.date,
                                std::time::Instant::now(),
                            );
                        }
                    } else if topic == args.insider_transactions_topic {
                        let txn = match UnifiedInsiderTransaction::decode(payload) {
                            Ok(t) => t,
                            Err(e) => {
                                warn!(
                                    "malformed UnifiedInsiderTransaction protobuf, skipping: {e}"
                                );
                                break 'handled "skipped_malformed";
                            }
                        };

                        // A transient database error is retried until the row is stored,
                        // and the offset is committed only after that (below): an
                        // insider transaction exists nowhere else once read, so a
                        // dropped one is lost. A row the schema itself rejects is
                        // logged and skipped: retrying it would block the partition.
                        if let Err(e) = insider_store::store_with_retry_counted(
                        "insider transaction",
                        || upsert_insider_filing(&pool, &txn),
                        INITIAL_STORE_BACKOFF,
                        MAX_STORE_BACKOFF,
                        || {
                            metrics::counter!("signal_messages_total", "topic" => topic.to_string(), "outcome" => "db_error_retried").increment(1);
                            metrics::counter!("signal_store_retries_total", "topic" => topic.to_string()).increment(1);
                        },
                    )
                    .await
                    {
                        error!(ticker = %txn.ticker, filer = %txn.person_name,
                               txn_date = %txn.transaction_date, error = %e,
                               "insider transaction rejected by the database, skipped");
                        metrics::counter!("signal_insider_db_errors_total").increment(1);
                        outcome = "db_error_committed";
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
                        outcome = "skipped_invalid";
                    }

                    outcome
                };

                metrics::counter!("signal_messages_total", "topic" => topic.to_string(), "outcome" => outcome)
                    .increment(1);
                metrics::histogram!("signal_store_duration_seconds", "topic" => topic.to_string())
                    .record(started.elapsed().as_secs_f64());
                metrics::gauge!("signal_last_processed_timestamp_seconds", "topic" => topic.to_string())
                    .set(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0.0, |d| d.as_secs_f64()),
                    );
                consumer.commit_message(&msg, CommitMode::Async).ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingProvider {
        calls: AtomicUsize,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("unexpected lookup")]
    struct Never;

    #[async_trait::async_trait]
    impl CompanyProvider for CountingProvider {
        type Error = Never;

        async fn sector(&self, _asset: &Asset) -> Result<Sector, Never> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Sector::Technology)
        }
    }

    fn provider() -> CountingProvider {
        CountingProvider {
            calls: AtomicUsize::new(0),
        }
    }

    #[tokio::test]
    async fn nordic_listing_skips_the_provider_and_has_no_sector() {
        let p = provider();
        let sector = resolve_sector(&p, &Asset::with_mic("GOMX", mic::FNSE))
            .await
            .unwrap();
        assert_eq!(sector, None);
        assert_eq!(p.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn us_listing_asks_the_provider() {
        let p = provider();
        let sector = resolve_sector(&p, &Asset::with_mic("AAPL", mic::XNAS))
            .await
            .unwrap();
        assert_eq!(sector, Some(Sector::Technology));
        assert_eq!(p.calls.load(Ordering::SeqCst), 1);
    }

    mod tracing_scope {
        use super::*;
        use rdkafka::message::{Header, OwnedHeaders, OwnedMessage, Timestamp};
        use telemetry::trace::testing::Harness;
        use tracing::instrument::WithSubscriber;

        const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

        fn report() -> SpecialEvent {
            SpecialEvent {
                ticker: "GOMX".into(),
                exchange_mic: "FNSE".into(),
                event_type: "quarterly_report".into(),
                occurred_at_unix_secs: 1_788_000_000,
                description: "Interim report".into(),
                source: reports::SOURCE_MFN.into(),
                source_id: "fa0cc072-6e67-4dc5-82c0-0bb587966b31".into(),
                url: "https://mfn.se/a".into(),
                lang: "en".into(),
                ..Default::default()
            }
        }

        fn message(topic: &str, event: &SpecialEvent, traceparent: Option<&str>) -> OwnedMessage {
            OwnedMessage::new(
                Some(event.encode_to_vec()),
                Some(b"GOMX:FNSE".to_vec()),
                topic.to_string(),
                Timestamp::NotAvailable,
                0,
                1,
                traceparent.map(|value| {
                    OwnedHeaders::new().insert(Header {
                        key: "traceparent",
                        value: Some(value),
                    })
                }),
            )
        }

        async fn store(pool: &PgPool, msg: &OwnedMessage, event: &SpecialEvent) {
            let span = release_span(msg, event);
            let consumer = span.unwrap_or_else(Span::none);
            let parsed = reports::parse(event).unwrap();
            reports::handle_in(pool, &parsed, &consumer).await.unwrap();
        }

        #[sqlx::test(migrations = "./migrations")]
        async fn a_release_with_a_traceparent_is_stored_in_its_trace(pool: PgPool) {
            crate::analyst_store::install_default_subscriber();
            let harness = Harness::new();
            let event = report();
            let traceparent = format!("00-{TRACE}-00f067aa0ba902b7-01");
            let msg = message("special.events", &event, Some(&traceparent));
            store(&pool, &msg, &event)
                .with_subscriber(harness.subscriber())
                .await;
            let consumer = harness.span("process special.events");
            assert_eq!(consumer.span_context.trace_id().to_string(), TRACE);
            assert_eq!(consumer.parent_span_id.to_string(), "00f067aa0ba902b7");
            let attribute = |key: &str| {
                consumer
                    .attributes
                    .iter()
                    .find(|a| a.key.as_str() == key)
                    .map(|a| a.value.to_string())
            };
            assert_eq!(attribute("nexus.ticker").as_deref(), Some("GOMX"));
            assert_eq!(
                attribute("nexus.event_type").as_deref(),
                Some("quarterly_report")
            );
            assert_eq!(
                attribute("mfn.news_id").as_deref(),
                Some("fa0cc072-6e67-4dc5-82c0-0bb587966b31")
            );
            let store = harness.span("signal.store_special_event");
            assert_eq!(store.parent_span_id, consumer.span_context.span_id());
            let recompute = harness.span("signal.reaction_recompute");
            assert_eq!(recompute.parent_span_id, consumer.span_context.span_id());
        }

        #[sqlx::test(migrations = "./migrations")]
        async fn a_release_without_a_traceparent_creates_no_spans(pool: PgPool) {
            crate::analyst_store::install_default_subscriber();
            let harness = Harness::new();
            let event = report();
            let msg = message("special.events", &event, None);
            store(&pool, &msg, &event)
                .with_subscriber(harness.subscriber())
                .await;
            assert_eq!(count_rows(&pool).await, 1);
            assert!(harness.spans().is_empty(), "{:?}", harness.spans());
        }

        #[sqlx::test(migrations = "./migrations")]
        async fn a_message_on_another_topic_creates_no_spans(pool: PgPool) {
            crate::analyst_store::install_default_subscriber();
            let harness = Harness::new();
            let registration = TickerRegistration {
                ticker: "GOMX".into(),
                exchange_mic: "FNSE".into(),
                currency: "SEK".into(),
                unregister: true,
                ..Default::default()
            };
            let outcome = handle_ticker_registration(
                &registration.encode_to_vec(),
                &pool,
                &super::provider(),
            )
            .with_subscriber(harness.subscriber())
            .await;
            assert_eq!(outcome, "skipped_invalid");
            assert!(harness.spans().is_empty(), "{:?}", harness.spans());
        }

        async fn count_rows(pool: &PgPool) -> i64 {
            sqlx::query_scalar("SELECT COUNT(*) FROM special_events WHERE source_id IS NOT NULL")
                .fetch_one(pool)
                .await
                .unwrap()
        }
    }

    mod report_store {
        use super::*;
        use std::sync::Mutex;

        fn store(
            op: impl FnMut() -> std::future::Ready<sqlx::Result<String>>,
        ) -> (Option<String>, Vec<bool>) {
            let blocked = Mutex::new(Vec::new());
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let out = rt.block_on(store_report(
                "special.events",
                "T",
                "report",
                "id",
                op,
                Duration::from_millis(1),
                |b| blocked.lock().unwrap().push(b),
            ));
            (out, blocked.into_inner().unwrap())
        }

        #[test]
        fn a_pool_timeout_is_retried_until_it_succeeds() {
            let calls = AtomicUsize::new(0);
            let (out, blocked) = store(|| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(if n < 2 {
                    Err(sqlx::Error::PoolTimedOut)
                } else {
                    Ok("stored".to_string())
                })
            });
            assert_eq!(out.as_deref(), Some("stored"));
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            assert_eq!(blocked, [true, true, false]);
        }

        #[test]
        fn a_rejected_row_is_committed_after_one_attempt() {
            let calls = AtomicUsize::new(0);
            let (out, blocked) = store(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err(sqlx::Error::RowNotFound))
            });
            assert_eq!(out, None);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(blocked, [false]);
        }
    }
}
