use model::{bar::Bar, generated::MarketEvent, generated::SpecialEvent};
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
            currency: bar.currency.clone(),
            timestamp_unix_secs: ts,
            exchange_mic: bar.asset.exchange_mic.clone(),
        };

        self.publish(topic, &bar.asset.ticker, &event).await
    }

    /// Publishes a `SpecialEvent` to `topic`, keyed by ticker.
    #[allow(dead_code)]
    pub async fn publish_special_event(
        &self,
        topic: &str,
        event: &SpecialEvent,
    ) -> Result<(), ProducerError> {
        self.publish(topic, &event.ticker, event).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use model::asset::{mic, Asset};

    fn make_bar(exchange_mic: &'static str) -> Bar {
        Bar {
            asset: Asset::with_mic("TEST", exchange_mic),
            open: 10.0,
            high: 11.0,
            low: 9.0,
            close: 10.5,
            volume: 5000.0,
            timestamp: SystemTime::UNIX_EPOCH,
            currency: mic::currency(exchange_mic).to_string(),
        }
    }

    #[test]
    fn usd_bar_encodes_usd_currency() {
        use model::generated::MarketEvent;
        use prost::Message;

        let bar = make_bar(mic::XNAS);
        // Simulate what publish_bar does when building the MarketEvent.
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
            currency: bar.currency.clone(), // must NOT be hardcoded "USD"
            timestamp_unix_secs: ts,
            exchange_mic: bar.asset.exchange_mic.clone(),
        };
        let encoded = event.encode_to_vec();
        let decoded = MarketEvent::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.currency, "USD");
    }

    #[test]
    fn sek_bar_encodes_sek_currency() {
        use model::generated::MarketEvent;
        use prost::Message;

        let bar = make_bar(mic::FNSE);
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
            currency: bar.currency.clone(), // must NOT be hardcoded "USD"
            timestamp_unix_secs: ts,
            exchange_mic: bar.asset.exchange_mic.clone(),
        };
        let encoded = event.encode_to_vec();
        let decoded = MarketEvent::decode(encoded.as_slice()).unwrap();
        assert_eq!(
            decoded.currency, "SEK",
            "SEK bar must not be silently mislabeled as USD"
        );
    }
}
