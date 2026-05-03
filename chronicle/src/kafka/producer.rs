use model::{bar::Bar, generated::MarketEvent};
use prost::Message;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use std::time::{Duration, SystemTime};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProducerError {
    #[error("kafka error: {0}")]
    Kafka(String),
}

pub struct ChronicleProducer {
    producer: FutureProducer,
}

impl ChronicleProducer {
    pub fn new(brokers: &str) -> Result<Self, ProducerError> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .create()
            .map_err(|e| ProducerError::Kafka(e.to_string()))?;

        Ok(Self { producer })
    }

    // Used by the `market` binary; the `chronicle` binary shares this module but
    // does not call this helper, so we suppress the dead-code lint here.
    #[allow(dead_code)]
    pub async fn publish_bar(&self, topic: &str, bar: &Bar) -> Result<(), ProducerError> {
        let ts = bar
            .timestamp
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let event = MarketEvent {
            ticker: bar.asset.ticker.clone(),
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume as u64,
            currency: "USD".to_string(),
            timestamp_unix_secs: ts,
        };

        self.publish(topic, &bar.asset.ticker, &event).await
    }

    /// Serialises `msg` as protobuf and publishes it to `topic`, keyed by `key`.
    pub async fn publish<M: Message>(
        &self,
        topic: &str,
        key: &str,
        msg: &M,
    ) -> Result<(), ProducerError> {
        let payload = msg.encode_to_vec();
        self.producer
            .send(
                FutureRecord::to(topic).key(key).payload(&payload),
                Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| ProducerError::Kafka(e.to_string()))?;
        Ok(())
    }
}
