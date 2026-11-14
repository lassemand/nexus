use std::time::Duration;

use async_trait::async_trait;
use model::generated::{AnalystDecision, AnalystTrade};
use prost::Message;
use rdkafka::error::KafkaError;
use rdkafka::message::{Headers, OwnedHeaders};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;

const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// The trace context of the current span as message headers: `traceparent`, and
/// `tracestate` when it is not empty. Empty outside a span with an OpenTelemetry
/// context, so a send without a span carries no trace headers. It is called
/// inside the `send analyst.decisions` span, so `signal` continues the same trace.
pub fn trace_headers() -> OwnedHeaders {
    telemetry::kafka::inject_current(OwnedHeaders::new())
}

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
        let headers = trace_headers();
        let mut record = FutureRecord::to(&self.topic).key(key).payload(&payload);
        if headers.count() > 0 {
            record = record.headers(headers);
        }
        self.producer
            .send(record, SEND_TIMEOUT)
            .await
            .map(|_| ())
            .map_err(|(error, _)| error.to_string())
    }
}

/// Where simulated trades are emitted after every state change.
#[async_trait]
pub trait TradeSink: Send + Sync {
    /// Emits the state of a trade keyed `ticker:exchange_mic`. An error is
    /// retried by the caller.
    async fn emit(&self, key: &str, trade: &AnalystTrade) -> Result<(), String>;
}

/// Produces `AnalystTrade` messages on a Kafka topic.
pub struct KafkaTradeSink {
    producer: FutureProducer,
    topic: String,
}

impl KafkaTradeSink {
    /// A producer for `topic`.
    pub fn new(brokers: &str, topic: &str) -> Result<KafkaTradeSink, KafkaError> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("linger.ms", "0")
            .set("enable.idempotence", "true")
            .set("message.timeout.ms", "5000")
            .create()?;
        Ok(KafkaTradeSink {
            producer,
            topic: topic.to_owned(),
        })
    }
}

#[async_trait]
impl TradeSink for KafkaTradeSink {
    async fn emit(&self, key: &str, trade: &AnalystTrade) -> Result<(), String> {
        let payload = trade.encode_to_vec();
        let headers = trace_headers();
        let mut record = FutureRecord::to(&self.topic).key(key).payload(&payload);
        if headers.count() > 0 {
            record = record.headers(headers);
        }
        self.producer
            .send(record, SEND_TIMEOUT)
            .await
            .map(|_| ())
            .map_err(|(error, _)| error.to_string())
    }
}
