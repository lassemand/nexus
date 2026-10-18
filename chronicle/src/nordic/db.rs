//! Postgres access for `report_releases`.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

/// A company eligible for Nasdaq news ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Company {
    pub ticker: String,
    pub exchange_mic: String,
    /// Exact issuer name in the Nasdaq news feed; `None` if unmapped.
    pub nasdaq_news_company: Option<String>,
}

/// A row to insert into `report_releases`.
#[derive(Debug, Clone)]
pub struct NewRelease<'a> {
    pub ticker: &'a str,
    pub exchange_mic: &'a str,
    pub disclosure_id: i64,
    pub released_at: DateTime<Utc>,
    pub category: &'a str,
    pub report_kind: Option<&'static str>,
    pub headline: &'a str,
    pub url: Option<&'a str>,
    pub is_correction: bool,
    pub calendar_update: bool,
}

/// Load all companies on the given MICs, mapped or not.
pub async fn load_companies(pool: &PgPool, mics: &[&str]) -> sqlx::Result<Vec<Company>> {
    let mics: Vec<String> = mics.iter().map(|m| m.to_string()).collect();
    let rows = sqlx::query(
        "SELECT ticker, exchange_mic, nasdaq_news_company FROM companies \
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
            nasdaq_news_company: r.get("nasdaq_news_company"),
        })
        .collect())
}

/// Insert a release; returns `true` if the row is new, `false` if the
/// `disclosure_id` was already stored (existing rows are left untouched).
pub async fn insert_release(pool: &PgPool, r: &NewRelease<'_>) -> sqlx::Result<bool> {
    let res = sqlx::query(
        "INSERT INTO report_releases \
         (ticker, exchange_mic, disclosure_id, released_at, category, report_kind, \
          headline, url, is_correction, calendar_update) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
         ON CONFLICT (disclosure_id) DO NOTHING",
    )
    .bind(r.ticker)
    .bind(r.exchange_mic)
    .bind(r.disclosure_id)
    .bind(r.released_at)
    .bind(r.category)
    .bind(r.report_kind)
    .bind(r.headline)
    .bind(r.url)
    .bind(r.is_correction)
    .bind(r.calendar_update)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Point each report's `superseded_by` at the latest correction published for
/// the same company within 7 days after it. Idempotent; safe to run every time.
pub async fn link_corrections(pool: &PgPool, ticker: &str, mic: &str) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE report_releases o SET superseded_by = c.id \
         FROM ( \
             SELECT DISTINCT ON (o2.id) o2.id AS original_id, c2.id AS id \
             FROM report_releases o2 \
             JOIN report_releases c2 \
               ON c2.ticker = o2.ticker AND c2.exchange_mic = o2.exchange_mic \
              AND c2.is_correction \
              AND c2.released_at > o2.released_at \
              AND c2.released_at <= o2.released_at + interval '7 days' \
             WHERE o2.ticker = $1 AND o2.exchange_mic = $2 \
               AND o2.report_kind IS NOT NULL AND NOT o2.is_correction \
             ORDER BY o2.id, c2.released_at DESC \
         ) c \
         WHERE o.id = c.original_id AND o.superseded_by IS DISTINCT FROM c.id",
    )
    .bind(ticker)
    .bind(mic)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
