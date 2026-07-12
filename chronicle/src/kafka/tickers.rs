use model::generated::TickerRegistration;
use prost::Message;
use rdkafka::{
    consumer::{BaseConsumer, Consumer},
    ClientConfig, Message as KafkaMessage, Offset, TopicPartitionList,
};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use tracing::{info, warn};

/// Reads all `TickerRegistration` messages from the `company.tickers` topic
/// from the very beginning and returns the de-duplicated set of ticker symbols.
///
/// Uses a short-lived `BaseConsumer` so there is no persistent group offset
/// committed — each call always re-reads from the start. Returns once the
/// watermark high offset is reached (i.e. the partition is fully drained) or
/// after `timeout` with no new messages.
// Used by chronicle, market, earnings, filings binaries — not all in the same compilation unit.
#[allow(dead_code)]
pub fn load_tickers(brokers: &str, topic: &str) -> Vec<String> {
    // Use a unique group so we never interfere with running consumers.
    let group_id = format!("chronicle-load-tickers-{}", std::process::id());

    let consumer: BaseConsumer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("group.id", &group_id)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()
        .expect("failed to create ticker-loader consumer");

    // Assign partition 0 from offset 0 explicitly so we always replay from start.
    let mut tpl = TopicPartitionList::new();
    tpl.add_partition_offset(topic, 0, Offset::Beginning)
        .expect("failed to set partition offset");
    consumer
        .assign(&tpl)
        .expect("failed to assign topic partition");

    // Fetch the high watermark to know when we've caught up.
    let (_, high) = consumer
        .fetch_watermarks(topic, 0, Duration::from_secs(5))
        .unwrap_or((0, 0));

    if high == 0 {
        info!(topic, "no ticker registrations found in topic");
        return vec![];
    }

    let mut tickers: HashSet<String> = HashSet::new();
    let idle_timeout = Duration::from_secs(3);
    let mut last_message = Instant::now();

    loop {
        // Stop once we've consumed all messages up to the high watermark.
        if tickers.len() as i64 >= high {
            break;
        }
        // Also stop if we've been idle for a while (handles sparse topics).
        if last_message.elapsed() > idle_timeout {
            break;
        }

        match consumer.poll(Duration::from_millis(200)) {
            Some(Ok(msg)) => {
                last_message = Instant::now();
                let Some(payload) = msg.payload() else {
                    continue;
                };
                match TickerRegistration::decode(payload) {
                    Ok(r) => {
                        tickers.insert(r.ticker.to_uppercase());
                    }
                    Err(e) => warn!("malformed TickerRegistration, skipping: {e}"),
                }
            }
            Some(Err(e)) => warn!("kafka poll error: {e}"),
            None => {}
        }
    }

    let mut result: Vec<String> = tickers.into_iter().collect();
    result.sort();
    info!(
        count = result.len(),
        topic, "loaded tickers from kafka topic"
    );
    result
}

/// Reads all `TickerRegistration` messages from the `company.tickers` topic
/// and returns the full registrations keyed by ticker symbol.
///
/// Unlike [`load_tickers`], this preserves `exchange_mic` so callers can
/// filter by exchange (e.g. only FNSE-listed instruments).
/// The latest registration wins when the same ticker appears more than once.
// Used by the pdmr binary — not all binaries use it.
#[allow(dead_code)]
pub fn load_ticker_registrations(
    brokers: &str,
    topic: &str,
) -> HashMap<String, TickerRegistration> {
    let group_id = format!("chronicle-load-registrations-{}", std::process::id());

    let consumer: BaseConsumer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("group.id", &group_id)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()
        .expect("failed to create ticker-registration loader consumer");

    let mut tpl = TopicPartitionList::new();
    tpl.add_partition_offset(topic, 0, Offset::Beginning)
        .expect("failed to set partition offset");
    consumer
        .assign(&tpl)
        .expect("failed to assign topic partition");

    let (_, high) = consumer
        .fetch_watermarks(topic, 0, Duration::from_secs(5))
        .unwrap_or((0, 0));

    if high == 0 {
        info!(topic, "no ticker registrations found in topic");
        return HashMap::new();
    }

    let mut registrations: HashMap<String, TickerRegistration> = HashMap::new();
    let idle_timeout = Duration::from_secs(3);
    let mut last_message = Instant::now();

    loop {
        if registrations.len() as i64 >= high {
            break;
        }
        if last_message.elapsed() > idle_timeout {
            break;
        }

        match consumer.poll(Duration::from_millis(200)) {
            Some(Ok(msg)) => {
                last_message = Instant::now();
                let Some(payload) = msg.payload() else {
                    continue;
                };
                match TickerRegistration::decode(payload) {
                    Ok(r) => {
                        registrations.insert(r.ticker.to_uppercase(), r);
                    }
                    Err(e) => warn!("malformed TickerRegistration, skipping: {e}"),
                }
            }
            Some(Err(e)) => warn!("kafka poll error: {e}"),
            None => {}
        }
    }

    info!(
        count = registrations.len(),
        topic, "loaded ticker registrations from kafka topic"
    );
    registrations
}
