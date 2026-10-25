//! Postgres access for the Nordic jobs: the companies to read and how far each
//! company's MFN feed and calendar have been published (`mfn_ingest_state`).
//! Reports themselves are stored only by signal.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

/// A company eligible for MFN news ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Company {
    pub ticker: String,
    pub exchange_mic: String,
    /// MFN slug (`https://mfn.se/a/<slug>`); `None` if unmapped.
    pub mfn_slug: Option<String>,
    /// Display name (`companies.name`), if set.
    pub name: Option<String>,
}

/// Load all companies on the given MICs, mapped or not.
pub async fn load_companies(pool: &PgPool, mics: &[&str]) -> sqlx::Result<Vec<Company>> {
    let mics: Vec<String> = mics.iter().map(|m| m.to_string()).collect();
    let rows = sqlx::query(
        "SELECT ticker, exchange_mic, mfn_slug, name FROM companies \
         WHERE exchange_mic = ANY($1) ORDER BY ticker",
    )
    .bind(&mics)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Company {
            ticker: r.get("ticker"),
            exchange_mic: r.get("exchange_mic"),
            mfn_slug: r.get("mfn_slug"),
            name: r.get("name"),
        })
        .collect())
}

/// What has been published for one company. Default when there is no row:
/// nothing published yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestState {
    /// `publish_date` of the newest MFN item published.
    pub feed_cursor: Option<DateTime<Utc>>,
    /// Hash of the last calendar snapshot published.
    pub calendar_hash: Option<String>,
}

/// The company's [`IngestState`].
pub async fn load_state(pool: &PgPool, ticker: &str, mic: &str) -> sqlx::Result<IngestState> {
    let row = sqlx::query(
        "SELECT feed_cursor, calendar_hash FROM mfn_ingest_state \
         WHERE ticker = $1 AND exchange_mic = $2",
    )
    .bind(ticker)
    .bind(mic)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(|r| IngestState {
            feed_cursor: r.get("feed_cursor"),
            calendar_hash: r.get("calendar_hash"),
        })
        .unwrap_or_default())
}

/// Moves the feed cursor forward to `cursor` (never backwards).
pub async fn advance_feed_cursor(
    pool: &PgPool,
    ticker: &str,
    mic: &str,
    cursor: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mfn_ingest_state (ticker, exchange_mic, feed_cursor) VALUES ($1, $2, $3) \
         ON CONFLICT (ticker, exchange_mic) DO UPDATE SET \
           feed_cursor = GREATEST(mfn_ingest_state.feed_cursor, EXCLUDED.feed_cursor), \
           updated_at = now()",
    )
    .bind(ticker)
    .bind(mic)
    .bind(cursor)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records the hash of the calendar snapshot just published.
pub async fn set_calendar_hash(
    pool: &PgPool,
    ticker: &str,
    mic: &str,
    hash: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mfn_ingest_state (ticker, exchange_mic, calendar_hash) VALUES ($1, $2, $3) \
         ON CONFLICT (ticker, exchange_mic) DO UPDATE SET \
           calendar_hash = EXCLUDED.calendar_hash, updated_at = now()",
    )
    .bind(ticker)
    .bind(mic)
    .bind(hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Every company with an MFN slug, on any exchange.
pub async fn load_mapped_companies(pool: &PgPool) -> sqlx::Result<Vec<Company>> {
    let rows = sqlx::query(
        "SELECT ticker, exchange_mic, mfn_slug, name FROM companies \
         WHERE mfn_slug IS NOT NULL ORDER BY ticker",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Company {
            ticker: r.get("ticker"),
            exchange_mic: r.get("exchange_mic"),
            mfn_slug: r.get("mfn_slug"),
            name: r.get("name"),
        })
        .collect())
}
