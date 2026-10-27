//! Company registry for the chronicle database.
//!
//! `nexus register` publishes `TickerRegistration`s to `company.tickers`, and
//! `nexus unregister` publishes the same message with `unregister` set. The
//! topic is replayed in order ([`replay`]): the last message for a listing
//! decides whether it is registered. The binaries that read `companies` (pdmr,
//! nordic-reports, nordic-calendar) call [`sync_from_topic`] first, so a
//! registered listing gets a row, and an unregistered one loses it, without
//! anyone editing the table by hand. Source mappings (`nasdaq_news_company`, `mfn_slug`,
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

/// Uppercase ticker and MIC (`XNAS` when none was sent) of a registration,
/// or `None` without a ticker.
fn listing_key(r: &TickerRegistration) -> Option<(String, String)> {
    let ticker = r.ticker.trim().to_uppercase();
    if ticker.is_empty() {
        return None;
    }
    let exchange_mic = if r.exchange_mic.is_empty() {
        mic::XNAS.to_string()
    } else {
        r.exchange_mic.clone()
    };
    Some((ticker, exchange_mic))
}

impl Listing {
    fn is(&self, ticker: &str, exchange_mic: &str) -> bool {
        self.ticker == ticker && self.exchange_mic == exchange_mic
    }

    /// Normalises a registration the same way signal does: uppercase ticker,
    /// `XNAS` when no MIC was sent, currency derived from the MIC when absent.
    /// Returns `None` for a registration without a ticker, or without a
    /// currency on a MIC whose currency is unknown (logged).
    pub fn from_registration(r: &TickerRegistration) -> Option<Self> {
        let (ticker, exchange_mic) = listing_key(r)?;
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

/// The outcome of replaying `company.tickers`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Registrations {
    /// Listings whose last message is a registration, in first-registered order.
    pub active: Vec<Listing>,
    /// `(ticker, exchange_mic)` of listings whose last message is an unregistration.
    pub withdrawn: Vec<(String, String)>,
}

impl Registrations {
    /// The distinct tickers of the active listings, sorted.
    pub fn tickers(&self) -> Vec<String> {
        let mut tickers: Vec<String> = self.active.iter().map(|l| l.ticker.clone()).collect();
        tickers.sort();
        tickers.dedup();
        tickers
    }
}

/// Replays registrations in log order. The last message for a listing wins;
/// a repeated registration keeps the first one's currency, like
/// [`upsert_listings`] does.
pub fn replay<I: IntoIterator<Item = TickerRegistration>>(messages: I) -> Registrations {
    let mut out = Registrations::default();
    for r in messages {
        if r.unregister {
            let Some((ticker, exchange_mic)) = listing_key(&r) else {
                continue;
            };
            out.active.retain(|l| !l.is(&ticker, &exchange_mic));
            if !out
                .withdrawn
                .contains(&(ticker.clone(), exchange_mic.clone()))
            {
                out.withdrawn.push((ticker, exchange_mic));
            }
        } else if let Some(listing) = Listing::from_registration(&r) {
            out.withdrawn.retain(|(t, m)| !listing.is(t, m));
            if !out
                .active
                .iter()
                .any(|l| l.is(&listing.ticker, &listing.exchange_mic))
            {
                out.active.push(listing);
            }
        }
    }
    out
}

/// Reads every message on partition 0 of `topic`, from the start, and
/// [`replay`]s them.
///
/// Blocking: call it through [`sync_from_topic`] (or `spawn_blocking`). Uses
/// manual assignment and never commits, so there is no consumer-group state.
pub fn read_registrations(brokers: &str, topic: &str) -> anyhow::Result<Registrations> {
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
        return Ok(Registrations::default());
    }

    let mut tpl = TopicPartitionList::new();
    tpl.add_partition_offset(topic, 0, Offset::Beginning)?;
    consumer.assign(&tpl)?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut messages = Vec::new();
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
                    Ok(r) => messages.push(r),
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
    Ok(replay(messages))
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

/// Deletes withdrawn listings from `companies`, with the per-company state
/// that references them (`mfn_ingest_state`, `saxo_daily_bars_state`), in one
/// transaction. Hand-set mappings go with the row. Returns the number of
/// companies deleted.
pub async fn delete_listings(pool: &PgPool, withdrawn: &[(String, String)]) -> sqlx::Result<u64> {
    let mut tx = pool.begin().await?;
    let mut deleted = 0;
    for (ticker, exchange_mic) in withdrawn {
        for state in [
            "DELETE FROM mfn_ingest_state WHERE ticker = $1 AND exchange_mic = $2",
            "DELETE FROM saxo_daily_bars_state WHERE ticker = $1 AND exchange_mic = $2",
        ] {
            sqlx::query(state)
                .bind(ticker)
                .bind(exchange_mic)
                .execute(&mut *tx)
                .await?;
        }
        deleted += sqlx::query("DELETE FROM companies WHERE ticker = $1 AND exchange_mic = $2")
            .bind(ticker)
            .bind(exchange_mic)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(deleted)
}

/// Brings `companies` in line with `topic`: inserts registered listings and
/// deletes unregistered ones. Returns the number of new rows.
pub async fn sync_from_topic(pool: &PgPool, brokers: &str, topic: &str) -> anyhow::Result<u64> {
    let (brokers, topic_owned) = (brokers.to_string(), topic.to_string());
    let registrations =
        tokio::task::spawn_blocking(move || read_registrations(&brokers, &topic_owned))
            .await
            .context("registry reader panicked")??;
    let inserted = upsert_listings(pool, &registrations.active).await?;
    let deleted = delete_listings(pool, &registrations.withdrawn).await?;
    info!(
        topic,
        registered = registrations.active.len(),
        unregistered = registrations.withdrawn.len(),
        inserted,
        deleted,
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
            unregister: false,
        }
    }

    fn unreg(ticker: &str, mic: &str) -> TickerRegistration {
        TickerRegistration {
            ticker: ticker.into(),
            exchange_mic: mic.into(),
            currency: String::new(),
            unregister: true,
        }
    }

    fn key(ticker: &str, mic: &str) -> (String, String) {
        (ticker.into(), mic.into())
    }

    #[test]
    fn last_message_for_a_listing_wins() {
        let r = replay([
            reg("GOMX", "FNSE", "SEK"),
            reg("trmd", "", ""),
            unreg("TRMD", "XNAS"),
        ]);
        assert_eq!(r.tickers(), vec!["GOMX"]);
        assert_eq!(r.withdrawn, vec![key("TRMD", "XNAS")]);

        let back = replay([
            reg("TRMD", "XNAS", "USD"),
            unreg("trmd", ""),
            reg("TRMD", "", ""),
        ]);
        assert_eq!(back.tickers(), vec!["TRMD"]);
        assert!(back.withdrawn.is_empty());
    }

    #[test]
    fn unregistration_only_touches_its_own_mic() {
        let r = replay([
            reg("NOVO-B", "XCSE", "DKK"),
            reg("NOVO-B", "FNDK", "DKK"),
            unreg("NOVO-B", "FNDK"),
        ]);
        assert_eq!(r.active.len(), 1);
        assert_eq!(r.active[0].exchange_mic, "XCSE");
        assert_eq!(r.withdrawn, vec![key("NOVO-B", "FNDK")]);
    }

    #[test]
    fn repeated_registrations_keep_the_first_and_skip_blank_tickers() {
        let r = replay([
            reg("GOMX", "FNSE", "SEK"),
            reg("GOMX", "FNSE", "EUR"),
            unreg(" ", "FNSE"),
        ]);
        assert_eq!(r.active.len(), 1);
        assert_eq!(r.active[0].currency, "SEK");
        assert!(r.withdrawn.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn delete_removes_the_company_and_its_state(pool: PgPool) -> sqlx::Result<()> {
        let listings: Vec<Listing> = [reg("TRMD", "XNAS", "USD"), reg("GOMX", "FNSE", "SEK")]
            .iter()
            .filter_map(Listing::from_registration)
            .collect();
        upsert_listings(&pool, &listings).await?;
        sqlx::query("INSERT INTO mfn_ingest_state (ticker, exchange_mic) VALUES ('TRMD', 'XNAS')")
            .execute(&pool)
            .await?;

        assert_eq!(delete_listings(&pool, &[key("TRMD", "XNAS")]).await?, 1);
        assert_eq!(delete_listings(&pool, &[key("TRMD", "XNAS")]).await?, 0);

        let left: Vec<String> = sqlx::query_scalar("SELECT ticker FROM companies ORDER BY ticker")
            .fetch_all(&pool)
            .await?;
        assert_eq!(left, vec!["GOMX"]);
        let state: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mfn_ingest_state")
            .fetch_one(&pool)
            .await?;
        assert_eq!(state, 0);
        Ok(())
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
