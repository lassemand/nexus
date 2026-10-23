//! Postgres access for `report_releases`.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{PgPool, Row};
use tracing::info;

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

/// What [`record_release`] did with a Nasdaq item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The `disclosure_id` was already stored; the row was refreshed in place.
    Updated,
    /// The item filled in a scheduled row; carries that row's `expected_date`.
    Matched(NaiveDate),
    /// A new row was inserted (expected fields NULL).
    Inserted,
}

/// Days either side of the release's Stockholm date within which a scheduled
/// row is considered to be the same report.
pub const MATCH_WINDOW_DAYS: i64 = 7;

/// Record one Nasdaq item in a single transaction.
///
/// 1. `disclosure_id` already stored → update that row in place (idempotent).
/// 2. A real report (`report_kind` set, not a correction, not a calendar
///    update) → fill the closest unreleased row of the same company whose
///    `expected_date` is within ±[`MATCH_WINDOW_DAYS`] of the release's
///    Europe/Stockholm date (ties: the earlier date), keeping its expected
///    fields.
/// 3. Otherwise insert a new row.
///
/// Corrections, non-reports and calendar updates never match a scheduled row.
pub async fn record_release(pool: &PgPool, r: &NewRelease<'_>) -> sqlx::Result<Recorded> {
    let mut tx = pool.begin().await?;

    let updated = sqlx::query(
        "UPDATE report_releases SET released_at = $2, category = $3, report_kind = $4, \
                headline = $5, url = $6, is_correction = $7, calendar_update = $8, \
                fetched_at = now() \
         WHERE disclosure_id = $1 RETURNING id",
    )
    .bind(r.disclosure_id)
    .bind(r.released_at)
    .bind(r.category)
    .bind(r.report_kind)
    .bind(r.headline)
    .bind(r.url)
    .bind(r.is_correction)
    .bind(r.calendar_update)
    .fetch_optional(&mut *tx)
    .await?;
    if updated.is_some() {
        tx.commit().await?;
        return Ok(Recorded::Updated);
    }

    if r.report_kind.is_some() && !r.is_correction && !r.calendar_update {
        let candidate = sqlx::query(
            "SELECT id, expected_date FROM report_releases \
             WHERE ticker = $1 AND exchange_mic = $2 AND released_at IS NULL \
               AND expected_date BETWEEN \
                   (($3::timestamptz AT TIME ZONE 'Europe/Stockholm')::date - $4::int) AND \
                   (($3::timestamptz AT TIME ZONE 'Europe/Stockholm')::date + $4::int) \
             ORDER BY abs(expected_date - ($3::timestamptz AT TIME ZONE 'Europe/Stockholm')::date), \
                      expected_date \
             LIMIT 1 FOR UPDATE",
        )
        .bind(r.ticker)
        .bind(r.exchange_mic)
        .bind(r.released_at)
        .bind(MATCH_WINDOW_DAYS as i32)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = candidate {
            let id: i64 = row.get("id");
            let expected: NaiveDate = row.get("expected_date");
            sqlx::query(
                "UPDATE report_releases SET disclosure_id = $2, released_at = $3, \
                        category = $4, report_kind = $5, headline = $6, url = $7, \
                        fetched_at = now() \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(r.disclosure_id)
            .bind(r.released_at)
            .bind(r.category)
            .bind(r.report_kind)
            .bind(r.headline)
            .bind(r.url)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            info!(
                ticker = r.ticker,
                expected_date = %expected,
                released_at = %r.released_at,
                "matched scheduled report"
            );
            return Ok(Recorded::Matched(expected));
        }
    }

    let inserted = sqlx::query(
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
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // A concurrent run stored the same disclosure between our UPDATE and INSERT.
    Ok(if inserted.rows_affected() == 1 {
        Recorded::Inserted
    } else {
        Recorded::Updated
    })
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
