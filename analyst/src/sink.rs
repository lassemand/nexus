use std::time::Duration;

use async_trait::async_trait;
use model::generated::AnalystDecision;
use prost::Message;
use rdkafka::error::KafkaError;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;

const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Where decisions are emitted once they are durable.
#[async_trait]
pub trait DecisionSink: Send + Sync {
    /// Emits a decision keyed `ticker:exchange_mic`. An error is retried by the
    /// caller.
    async fn emit(&self, key: &str, decision: &AnalystDecision) -> Result<(), String>;
}

/// Produces `AnalystDecision` messages on a Kafka topic.
pub struct KafkaDecisionSink {
    producer: FutureProducer,
    topic: String,
}

impl KafkaDecisionSink {
    /// A producer for `topic`. `linger.ms` is 0: a decision must not wait to be
    /// batched with others.
    pub fn new(brokers: &str, topic: &str) -> Result<KafkaDecisionSink, KafkaError> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("linger.ms", "0")
            .set("enable.idempotence", "true")
            .set("message.timeout.ms", "5000")
            .create()?;
        Ok(KafkaDecisionSink {
            producer,
            topic: topic.to_owned(),
        })
    }
}

#[async_trait]
impl DecisionSink for KafkaDecisionSink {
    async fn emit(&self, key: &str, decision: &AnalystDecision) -> Result<(), String> {
        let payload = decision.encode_to_vec();
        let record = FutureRecord::to(&self.topic).key(key).payload(&payload);
        self.producer
            .send(record, SEND_TIMEOUT)
            .await
            .map(|_| ())
            .map_err(|(error, _)| error.to_string())
    }
}
