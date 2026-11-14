use std::sync::{Arc, Mutex};
use std::time::Duration;

use model::generated::SpecialEvent;
use prost::Message;
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::error::KafkaError;
use rdkafka::topic_partition_list::{Offset, TopicPartitionList};
use rdkafka::{ClientConfig, Message as KafkaMessage};
use telemetry::trace::OpenTelemetrySpanExt;
use tokio::task::JoinSet;
use tracing::field::Empty;
use tracing::{debug, info, info_span, warn, Instrument, Span};

use crate::metrics;
use crate::offsets::OffsetTracker;
use crate::release::{classify, Classified, Receipt, ReleaseInput, ReleaseProcessor};

/// The Kafka consumer group of the release path.
pub const RELEASES_GROUP: &str = "analyst-releases";

const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const ERROR_PAUSE: Duration = Duration::from_secs(1);

/// Consumes report releases from `topic` and decides on each, until `shutdown`
/// completes.
///
/// The group is `analyst-releases` and a first start reads from the **latest**
/// offset: the poll fallback replays history, and replayed reports must not open
/// trades. Every release that passes the filter is processed in its own task, so
/// one company's slow write never holds another's; offsets are committed only
/// when everything before them is done ([`OffsetTracker`]). Events that do not
/// pass the filter are counted and their offsets are done at once.
pub async fn run_release_consumer(
    brokers: &str,
    topic: &str,
    processor: Arc<ReleaseProcessor>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), KafkaError> {
    let consumer: Arc<StreamConsumer> = Arc::new(
        ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("group.id", RELEASES_GROUP)
            .set("auto.offset.reset", "latest")
            .set("enable.auto.commit", "false")
            .create()?,
    );
    consumer.subscribe(&[topic])?;
    info!(%brokers, %topic, group = RELEASES_GROUP, "release consumer started");
    let tracker = Arc::new(Mutex::new(OffsetTracker::new()));
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        let message = tokio::select! {
            () = &mut shutdown => break,
            received = consumer.recv() => received,
        };
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                warn!(%error, "release consumer error");
                tokio::time::sleep(ERROR_PAUSE).await;
                continue;
            }
        };
        let receipt = Receipt::now(processor.clock().as_ref());
        let (partition, offset) = (message.partition(), message.offset());
        lock(&tracker).begin(topic, partition, offset);
        match prepare(&message) {
            Prepared::Ignore(reason) => {
                metrics::release_event(&format!("ignored_{reason}"));
                finish(&consumer, &tracker, topic, partition, offset);
            }
            Prepared::Process(input, span) => {
                let processor = Arc::clone(&processor);
                let consumer = Arc::clone(&consumer);
                let tracker = Arc::clone(&tracker);
                let topic = topic.to_owned();
                let task_span = span.clone();
                tasks.spawn(
                    async move {
                        processor.process(*input, receipt).await;
                        let commit = info_span!("analyst.commit_offset", otel.kind = "internal");
                        commit.in_scope(|| finish(&consumer, &tracker, &topic, partition, offset));
                    }
                    .instrument(task_span),
                );
            }
        }
        while tasks.try_join_next().is_some() {}
    }
    info!(in_flight = tasks.len(), "release consumer stopping");
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    Ok(())
}

/// What the consumer does with a message.
pub enum Prepared {
    /// Not for the analyst; the reason labels the ignored-event metric.
    Ignore(&'static str),
    /// A release to decide on, with its `process special.events` span.
    Process(Box<ReleaseInput>, Span),
}

/// Decodes and classifies `message`. Only a release that passes the filter gets a
/// span, so the thousands of other events on the topic cost no traces.
pub fn prepare<M: KafkaMessage>(message: &M) -> Prepared {
    let event = match message.payload().map(SpecialEvent::decode) {
        Some(Ok(event)) => event,
        Some(Err(error)) => {
            debug!(%error, partition = message.partition(), offset = message.offset(), "undecodable event");
            return Prepared::Ignore("undecodable");
        }
        None => return Prepared::Ignore("no_payload"),
    };
    match classify(&event) {
        Classified::Ignore(reason) => Prepared::Ignore(reason),
        Classified::Process(input) => {
            let span = consumer_span(message, &input);
            Prepared::Process(input, span)
        }
    }
}

/// The `process special.events` span of a release: a consumer span whose parent
/// is the trace context in the message headers, so the release's trace from
/// `nordic-live` continues here. A message without a valid `traceparent` starts a
/// root trace and is marked `nexus.trace_parent_missing`.
pub fn consumer_span<M: KafkaMessage>(message: &M, input: &ReleaseInput) -> Span {
    let key = message
        .key()
        .map(|key| String::from_utf8_lossy(key).into_owned())
        .unwrap_or_default();
    let span = info_span!(
        "process special.events",
        otel.kind = "consumer",
        messaging.system = "kafka",
        messaging.destination.name = message.topic(),
        messaging.kafka.message.key = key.as_str(),
        messaging.kafka.message.offset = message.offset(),
        messaging.destination.partition.id = message.partition(),
        nexus.ticker = input.ticker.as_str(),
        nexus.exchange_mic = input.exchange_mic.as_str(),
        mfn.news_id = input.news_id.as_str(),
        nexus.lang = input.lang.as_str(),
        nexus.fiscal_period = input.fiscal_period.as_deref().unwrap_or(""),
        nexus.decision = Empty,
        nexus.trace_parent_missing = Empty
    );
    let linked = telemetry::kafka::extract(message)
        .map(|parent| span.set_parent(parent).is_ok())
        .unwrap_or(false);
    if !linked {
        span.record("nexus.trace_parent_missing", true);
    }
    span
}

fn lock(tracker: &Mutex<OffsetTracker>) -> std::sync::MutexGuard<'_, OffsetTracker> {
    tracker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn finish(
    consumer: &StreamConsumer,
    tracker: &Mutex<OffsetTracker>,
    topic: &str,
    partition: i32,
    offset: i64,
) {
    let Some(next) = lock(tracker).done(topic, partition, offset) else {
        return;
    };
    let mut list = TopicPartitionList::new();
    if let Err(error) = list.add_partition_offset(topic, partition, Offset::Offset(next)) {
        warn!(%error, "could not build the offset to commit");
        return;
    }
    if let Err(error) = consumer.commit(&list, CommitMode::Async) {
        warn!(%error, partition, next, "could not commit the release offset");
    }
}
