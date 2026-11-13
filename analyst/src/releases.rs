use std::sync::{Arc, Mutex};
use std::time::Duration;

use model::generated::SpecialEvent;
use prost::Message;
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::error::KafkaError;
use rdkafka::topic_partition_list::{Offset, TopicPartitionList};
use rdkafka::{ClientConfig, Message as KafkaMessage};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::metrics;
use crate::offsets::OffsetTracker;
use crate::release::{classify, Classified, Receipt, ReleaseProcessor};

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
        let classified = match message.payload().map(SpecialEvent::decode) {
            Some(Ok(event)) => classify(&event),
            Some(Err(error)) => {
                debug!(%error, partition, offset, "undecodable event");
                Classified::Ignore("undecodable")
            }
            None => Classified::Ignore("no_payload"),
        };
        match classified {
            Classified::Ignore(reason) => {
                metrics::release_event(&format!("ignored_{reason}"));
                finish(&consumer, &tracker, topic, partition, offset);
            }
            Classified::Process(input) => {
                let processor = Arc::clone(&processor);
                let consumer = Arc::clone(&consumer);
                let tracker = Arc::clone(&tracker);
                let topic = topic.to_owned();
                tasks.spawn(async move {
                    processor.process(*input, receipt).await;
                    finish(&consumer, &tracker, &topic, partition, offset);
                });
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
