//! Publishes Nordic reports as `SpecialEvent`s (NEX-171).
//!
//! `report_releases` stays chronicle's working table; signal stores the events
//! in `special_events`. Each periodic report (`report_kind` set, not a
//! correction) becomes one keyed event, `report:<id>`, that is re-published
//! whenever it changes (date moved, released) and removed when its row is
//! gone (e.g. a calendar refresh replaced a scheduled row).
//!
//! Outbox style: [`sync`] compares the current reports with
//! `published_report_events`, publishes the difference, and records each
//! event after it was delivered. A failed publish leaves the rest pending for
//! the next run, so delivery is at-least-once and the consumer is idempotent.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use model::generated::SpecialEvent;
use prost::Message;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::Duration;
use tracing::info;

/// Default topic signal consumes special events from.
pub const SPECIAL_EVENTS_TOPIC: &str = "special.events";

/// A report as a special event.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ReportEvent {
    /// `report:<report_releases.id>`.
    pub source_key: String,
    pub ticker: String,
    /// `quarterly report` or `annual report`.
    pub event_type: String,
    /// Release instant, or the expected one (Europe/Stockholm) while scheduled.
    pub occurred_at: DateTime<Utc>,
    pub description: String,
    /// Not released yet.
    pub scheduled: bool,
}

impl ReportEvent {
    fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.ticker,
            self.event_type,
            self.occurred_at.to_rfc3339(),
            self.description,
            self.scheduled
        )
    }

    fn to_proto(&self) -> SpecialEvent {
        SpecialEvent {
            ticker: self.ticker.clone(),
            event_type: self.event_type.clone(),
            occurred_at_unix_secs: self.occurred_at.timestamp(),
            description: self.description.clone(),
            source_key: self.source_key.clone(),
            scheduled: self.scheduled,
            removed: false,
        }
    }
}

/// Where events go. Kafka in production ([`KafkaSink`]), a vector in tests.
#[async_trait]
pub trait EventSink {
    /// Delivers one event; returns once the broker acknowledged it.
    async fn publish(&self, event: &SpecialEvent) -> anyhow::Result<()>;
}

/// Publishes to a Kafka topic, keyed by ticker like the other chronicle events.
pub struct KafkaSink {
    producer: FutureProducer,
    topic: String,
}

impl KafkaSink {
    /// Connects a producer for `topic` on `brokers`.
    pub fn new(brokers: &str, topic: impl Into<String>) -> anyhow::Result<Self> {
        let producer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "10000")
            .create()?;
        Ok(Self {
            producer,
            topic: topic.into(),
        })
    }
}

#[async_trait]
impl EventSink for KafkaSink {
    async fn publish(&self, event: &SpecialEvent) -> anyhow::Result<()> {
        let payload = event.encode_to_vec();
        self.producer
            .send(
                FutureRecord::to(&self.topic)
                    .key(&event.ticker)
                    .payload(&payload),
                Duration::from_secs(10),
            )
            .await
            .map_err(|(e, _)| anyhow::anyhow!("publishing {}: {e}", event.source_key))?;
        Ok(())
    }
}

/// What one [`sync`] delivered.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Synced {
    /// New or changed events published.
    pub upserted: usize,
    /// Removals published.
    pub removed: usize,
}

/// Every periodic report, as the event it should be published as.
pub async fn report_events(pool: &PgPool) -> sqlx::Result<Vec<ReportEvent>> {
    sqlx::query_as(
        "SELECT
           'report:' || r.id AS source_key,
           r.ticker,
           CASE r.report_kind WHEN 'annual' THEN 'annual report' ELSE 'quarterly report' END
             AS event_type,
           COALESCE(r.occurred_at,
                    (r.expected_date + COALESCE(r.expected_time, TIME '00:00'))
                      AT TIME ZONE 'Europe/Stockholm') AS occurred_at,
           concat_ws(' · ',
             r.fiscal_period,
             COALESCE(r.headline, r.calendar_label),
             CASE WHEN r.occurred_at IS NULL AND r.expected_time IS NOT NULL
                  THEN 'time ' || to_char(r.expected_time, 'HH24:MI') || ' (estimated)' END)
             AS description,
           r.occurred_at IS NULL AS scheduled
         FROM report_releases r
         WHERE r.report_kind IS NOT NULL AND NOT r.is_correction
         ORDER BY r.id",
    )
    .fetch_all(pool)
    .await
}

/// Publishes every report event that is new, changed or gone since the last
/// sync, recording each one after delivery. Stops at the first failed publish.
pub async fn sync(pool: &PgPool, sink: &dyn EventSink) -> anyhow::Result<Synced> {
    let current = report_events(pool).await?;
    let published: HashMap<String, String> = sqlx::query_as::<_, (String, String)>(
        "SELECT source_key, fingerprint FROM published_report_events",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();

    let mut out = Synced::default();
    for event in &current {
        let fingerprint = event.fingerprint();
        if published.get(&event.source_key) == Some(&fingerprint) {
            continue;
        }
        sink.publish(&event.to_proto()).await?;
        sqlx::query(
            "INSERT INTO published_report_events (source_key, fingerprint) VALUES ($1, $2) \
             ON CONFLICT (source_key) DO UPDATE SET fingerprint = EXCLUDED.fingerprint, \
             published_at = now()",
        )
        .bind(&event.source_key)
        .bind(&fingerprint)
        .execute(pool)
        .await?;
        out.upserted += 1;
    }

    let live: std::collections::HashSet<&str> =
        current.iter().map(|e| e.source_key.as_str()).collect();
    for key in published.keys().filter(|k| !live.contains(k.as_str())) {
        sink.publish(&SpecialEvent {
            source_key: key.clone(),
            removed: true,
            ..Default::default()
        })
        .await?;
        sqlx::query("DELETE FROM published_report_events WHERE source_key = $1")
            .bind(key)
            .execute(pool)
            .await?;
        out.removed += 1;
    }

    info!(
        upserted = out.upserted,
        removed = out.removed,
        "report special events synced"
    );
    Ok(out)
}

/// [`sync`] to Kafka for the Nordic binaries. A failure is logged, not
/// returned: the run's own work is already stored, and whatever was not
/// delivered stays pending for the next run.
pub async fn sync_or_log(pool: &PgPool, brokers: Option<&str>, topic: &str) {
    let Some(brokers) = brokers else {
        tracing::warn!("KAFKA_BROKERS not set, report special events not published");
        return;
    };
    let result = match KafkaSink::new(brokers, topic) {
        Ok(sink) => sync(pool, &sink).await,
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        tracing::error!(error = %e, "publishing report special events failed, retrying next run");
    }
}
