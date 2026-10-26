//! Company registry for the chronicle database.
//!
//! `nexus register` publishes `TickerRegistration`s to `company.tickers`. The
//! binaries that read `companies` (pdmr, nordic-reports, nordic-calendar) call
//! [`sync_from_topic`] first, so a registered listing gets a row without anyone
//! inserting it by hand. Source mappings (`nasdaq_news_company`, `mfn_slug`,
//! `fi_issuer`) are still set by hand, see `docs/guides/nordic-companies.md`.

use std::time::{Duration, Instant};

use anyhow::Context;
use model::asset::mic;
use model::generated::TickerRegistration;
use prost::Message;
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::{ClientConfig, Message as _, Offset, TopicPartitionList};
use sqlx::PgPool;
use tracing::{info, warn};

/// Default topic `nexus register` publishes to.
pub const TICKERS_TOPIC: &str = "company.tickers";

/// A listing as registered on `company.tickers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// Uppercase ticker symbol, e.g. `GOMX`.
    pub ticker: String,
    /// ISO 10383 MIC, e.g. `FNSE`.
    pub exchange_mic: String,
    /// ISO 4217 trading currency, e.g. `SEK`.
    pub currency: String,
}

impl Listing {
    /// Normalises a registration the same way signal does: uppercase ticker,
    /// `XNAS` when no MIC was sent, currency derived from the MIC when absent.
    /// Returns `None` for a registration without a ticker, or without a
    /// currency on a MIC whose currency is unknown (logged).
    pub fn from_registration(r: &TickerRegistration) -> Option<Self> {
        let ticker = r.ticker.trim().to_uppercase();
        if ticker.is_empty() {
            return None;
        }
        let exchange_mic = if r.exchange_mic.is_empty() {
            mic::XNAS.to_string()
        } else {
            r.exchange_mic.clone()
        };
        let currency = if r.currency.is_empty() {
            match mic::currency(&exchange_mic) {
                Some(c) => c.to_string(),
                None => {
                    tracing::warn!(
                        ticker = %ticker,
                        exchange_mic = %exchange_mic,
                        "skipping registration: no currency given and MIC has no known currency"
                    );
                    return None;
                }
            }
        } else {
            r.currency.clone()
        };
        Some(Self {
            ticker,
            exchange_mic,
            currency,
        })
    }
}

/// Reads every registration on partition 0 of `topic`, from the start.
///
/// Blocking: call it through [`sync_from_topic`] (or `spawn_blocking`). Uses
/// manual assignment and never commits, so there is no consumer-group state.
pub fn read_listings(brokers: &str, topic: &str) -> anyhow::Result<Vec<Listing>> {
    let consumer: BaseConsumer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set(
            "group.id",
            format!("chronicle-registry-{}", std::process::id()),
        )
        .set("enable.auto.commit", "false")
        .create()
        .context("creating the registry consumer")?;

    let (low, high) = consumer
        .fetch_watermarks(topic, 0, Duration::from_secs(10))
        .with_context(|| format!("reading the watermarks of {topic}"))?;
    if high <= low {
        return Ok(Vec::new());
    }

    let mut tpl = TopicPartitionList::new();
    tpl.add_partition_offset(topic, 0, Offset::Beginning)?;
    consumer.assign(&tpl)?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut listings = Vec::new();
    let mut next = low;
    while next < high {
        if Instant::now() > deadline {
            anyhow::bail!("timed out reading {topic} at offset {next} of {high}");
        }
        match consumer.poll(Duration::from_millis(500)) {
            Some(Ok(msg)) => {
                next = msg.offset() + 1;
                let Some(payload) = msg.payload() else {
                    continue;
                };
                match TickerRegistration::decode(payload) {
                    Ok(r) => listings.extend(Listing::from_registration(&r)),
                    Err(e) => warn!(
                        offset = msg.offset(),
                        "malformed TickerRegistration, skipping: {e}"
                    ),
                }
            }
            Some(Err(e)) => warn!("kafka poll error: {e}"),
            None => {}
        }
    }
    Ok(listings)
}

/// Inserts listings that are not in `companies` yet. Existing rows, and their
/// hand-set mappings, are left untouched. Returns the number of new rows.
pub async fn upsert_listings(pool: &PgPool, listings: &[Listing]) -> sqlx::Result<u64> {
    let mut inserted = 0;
    for l in listings {
        inserted += sqlx::query(
            "INSERT INTO companies (ticker, exchange_mic, currency) VALUES ($1, $2, $3) \
             ON CONFLICT (ticker, exchange_mic) DO NOTHING",
        )
        .bind(&l.ticker)
        .bind(&l.exchange_mic)
        .bind(&l.currency)
        .execute(pool)
        .await?
        .rows_affected();
    }
    Ok(inserted)
}

/// Copies every registration on `topic` into `companies`. Returns the number
/// of new rows.
pub async fn sync_from_topic(pool: &PgPool, brokers: &str, topic: &str) -> anyhow::Result<u64> {
    let (brokers, topic_owned) = (brokers.to_string(), topic.to_string());
    let listings = tokio::task::spawn_blocking(move || read_listings(&brokers, &topic_owned))
        .await
        .context("registry reader panicked")??;
    let inserted = upsert_listings(pool, &listings).await?;
    info!(
        topic,
        registrations = listings.len(),
        inserted,
        "companies synced from registrations"
    );
    Ok(inserted)
}

/// [`sync_from_topic`] for binaries that should carry on with the rows they
/// already have when Kafka is unavailable: failures are logged, not returned.
pub async fn sync_or_warn(pool: &PgPool, brokers: Option<&str>, topic: &str) {
    let Some(brokers) = brokers else {
        info!("KAFKA_BROKERS not set, using the companies table as it is");
        return;
    };
    if let Err(e) = sync_from_topic(pool, brokers, topic).await {
        warn!(error = %e, "could not sync companies from registrations, using the table as it is");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(ticker: &str, mic: &str, currency: &str) -> TickerRegistration {
        TickerRegistration {
            ticker: ticker.into(),
            exchange_mic: mic.into(),
            currency: currency.into(),
        }
    }

    #[test]
    fn registration_is_normalised_like_signal_does() {
        assert_eq!(
            Listing::from_registration(&reg("gomx", "FNSE", "")),
            Some(Listing {
                ticker: "GOMX".into(),
                exchange_mic: "FNSE".into(),
                currency: "SEK".into(),
            })
        );
        let us = Listing::from_registration(&reg("AAPL", "", "")).unwrap();
        assert_eq!(
            (us.exchange_mic.as_str(), us.currency.as_str()),
            ("XNAS", "USD")
        );
        assert_eq!(Listing::from_registration(&reg(" ", "FNSE", "SEK")), None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_inserts_once_and_keeps_mappings(pool: PgPool) -> sqlx::Result<()> {
        let gomx = Listing::from_registration(&reg("GOMX", "FNSE", "SEK")).unwrap();
        assert_eq!(
            upsert_listings(&pool, std::slice::from_ref(&gomx)).await?,
            1
        );

        sqlx::query("UPDATE companies SET fi_issuer = 'GomSpace Group AB' WHERE ticker = 'GOMX'")
            .execute(&pool)
            .await?;
        assert_eq!(upsert_listings(&pool, &[gomx.clone(), gomx]).await?, 0);

        let (sector, fi): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT sector, fi_issuer FROM companies WHERE ticker = 'GOMX'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(sector, None);
        assert_eq!(fi.as_deref(), Some("GomSpace Group AB"));
        Ok(())
    }
}
