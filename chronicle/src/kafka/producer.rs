use prost::Message;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use std::time::Duration;
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
