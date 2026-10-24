//! Postgres access for `report_releases`.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{PgPool, Row};
use tracing::info;
use uuid::Uuid;

/// A company eligible for MFN news ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Company {
    pub ticker: String,
    pub exchange_mic: String,
    /// MFN slug (`https://mfn.se/a/<slug>`); `None` if unmapped.
    pub mfn_slug: Option<String>,
}

/// A row to record in `report_releases`.
#[derive(Debug, Clone)]
pub struct NewRelease<'a> {
    pub ticker: &'a str,
    pub exchange_mic: &'a str,
    pub mfn_news_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub report_kind: Option<&'static str>,
    pub headline: &'a str,
    pub url: Option<&'a str>,
    pub lang: Option<&'a str>,
    pub tags: &'a [String],
    pub fiscal_period: Option<&'a str>,
    pub is_correction: bool,
    pub calendar_update: bool,
}

/// Load all companies on the given MICs, mapped or not.
pub async fn load_companies(pool: &PgPool, mics: &[&str]) -> sqlx::Result<Vec<Company>> {
    let mics: Vec<String> = mics.iter().map(|m| m.to_string()).collect();
    let rows = sqlx::query(
        "SELECT ticker, exchange_mic, mfn_slug FROM companies \
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
        })
        .collect())
}

/// Newest stored release instant for a company, if any.
pub async fn newest_occurred_at(
    pool: &PgPool,
    ticker: &str,
    mic: &str,
) -> sqlx::Result<Option<DateTime<Utc>>> {
    sqlx::query_scalar(
        "SELECT max(occurred_at) FROM report_releases WHERE ticker = $1 AND exchange_mic = $2",
    )
    .bind(ticker)
    .bind(mic)
    .fetch_one(pool)
    .await
}

/// What [`record_release`] did with an MFN item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The `mfn_news_id` was already stored; display fields were refreshed.
    Updated,
    /// The item is the other-language twin of a stored release (same company,
    /// instant and kind); no new row (an English item replaces a non-English one).
    Merged,
    /// The item filled in a scheduled row; carries that row's `expected_date`.
    Matched(NaiveDate),
    /// A new row was inserted (expected fields NULL).
    Inserted,
}

impl Recorded {
    /// True if the item was not seen before (a new row or a filled schedule).
    pub fn is_new(self) -> bool {
        matches!(self, Recorded::Matched(_) | Recorded::Inserted)
    }
}

/// Days either side of the release's Stockholm date within which a scheduled
/// row is considered to be the same report.
pub const MATCH_WINDOW_DAYS: i64 = 7;

/// Record one MFN item in a single transaction.
///
/// 1. `mfn_news_id` already stored → refresh display fields in place
///    (classification is decided once, at insert).
/// 2. A release of the same company, instant and `report_kind` is already stored
///    (the sv/en twin) → no second row; an English item replaces a stored
///    non-English one (`mfn_news_id`, `headline`, `url`, `lang`, `tags`).
/// 3. A real report (`report_kind` set; never a correction or calendar update)
///    → fill the closest scheduled row of the same company whose
///    `expected_date` is within ±[`MATCH_WINDOW_DAYS`] of the release's
///    Europe/Stockholm date (ties: the earlier date), keeping its expected fields.
/// 4. Otherwise insert a new row.
pub async fn record_release(pool: &PgPool, r: &NewRelease<'_>) -> sqlx::Result<Recorded> {
    let mut tx = pool.begin().await?;

    let updated = sqlx::query(
        "UPDATE report_releases SET headline = $2, url = $3, lang = $4, tags = $5, \
                fiscal_period = COALESCE(fiscal_period, $6), fetched_at = now() \
         WHERE mfn_news_id = $1 RETURNING id",
    )
    .bind(r.mfn_news_id)
    .bind(r.headline)
    .bind(r.url)
    .bind(r.lang)
    .bind(r.tags)
    .bind(r.fiscal_period)
    .fetch_optional(&mut *tx)
    .await?;
    if updated.is_some() {
        tx.commit().await?;
        return Ok(Recorded::Updated);
    }

    let is_report = r.report_kind.is_some() && !r.is_correction && !r.calendar_update;
    if is_report {
        let twin = sqlx::query(
            "SELECT id, lang FROM report_releases \
             WHERE ticker = $1 AND exchange_mic = $2 AND occurred_at = $3 AND report_kind = $4 \
             FOR UPDATE",
        )
        .bind(r.ticker)
        .bind(r.exchange_mic)
        .bind(r.occurred_at)
        .bind(r.report_kind)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = twin {
            let id: i64 = row.get("id");
            let stored_lang: Option<String> = row.get("lang");
            if stored_lang.as_deref() != Some("en") && r.lang == Some("en") {
                sqlx::query(
                    "UPDATE report_releases SET mfn_news_id = $2, headline = $3, url = $4, \
                            lang = $5, tags = $6, fetched_at = now() WHERE id = $1",
                )
                .bind(id)
                .bind(r.mfn_news_id)
                .bind(r.headline)
                .bind(r.url)
                .bind(r.lang)
                .bind(r.tags)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            return Ok(Recorded::Merged);
        }

        let candidate = sqlx::query(
            "SELECT id, expected_date FROM report_releases \
             WHERE ticker = $1 AND exchange_mic = $2 AND occurred_at IS NULL \
               AND expected_date BETWEEN \
                   (($3::timestamptz AT TIME ZONE 'Europe/Stockholm')::date - $4::int) AND \
                   (($3::timestamptz AT TIME ZONE 'Europe/Stockholm')::date + $4::int) \
             ORDER BY abs(expected_date - ($3::timestamptz AT TIME ZONE 'Europe/Stockholm')::date), \
                      expected_date \
             LIMIT 1 FOR UPDATE",
        )
        .bind(r.ticker)
        .bind(r.exchange_mic)
        .bind(r.occurred_at)
        .bind(MATCH_WINDOW_DAYS as i32)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = candidate {
            let id: i64 = row.get("id");
            let expected: NaiveDate = row.get("expected_date");
            sqlx::query(
                "UPDATE report_releases SET mfn_news_id = $2, occurred_at = $3, \
                        report_kind = $4, headline = $5, url = $6, lang = $7, tags = $8, \
                        fiscal_period = COALESCE(fiscal_period, $9), fetched_at = now() \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(r.mfn_news_id)
            .bind(r.occurred_at)
            .bind(r.report_kind)
            .bind(r.headline)
            .bind(r.url)
            .bind(r.lang)
            .bind(r.tags)
            .bind(r.fiscal_period)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            info!(
                ticker = r.ticker,
                expected_date = %expected,
                occurred_at = %r.occurred_at,
                "matched scheduled report"
            );
            return Ok(Recorded::Matched(expected));
        }
    }

    let inserted = sqlx::query(
        "INSERT INTO report_releases \
         (ticker, exchange_mic, mfn_news_id, occurred_at, report_kind, headline, url, lang, \
          tags, fiscal_period, is_correction, calendar_update) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
         ON CONFLICT DO NOTHING",
    )
    .bind(r.ticker)
    .bind(r.exchange_mic)
    .bind(r.mfn_news_id)
    .bind(r.occurred_at)
    .bind(r.report_kind)
    .bind(r.headline)
    .bind(r.url)
    .bind(r.lang)
    .bind(r.tags)
    .bind(r.fiscal_period)
    .bind(r.is_correction)
    .bind(r.calendar_update)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // A concurrent run stored the same item or its twin between our reads and the INSERT.
    Ok(if inserted.rows_affected() == 1 {
        Recorded::Inserted
    } else {
        Recorded::Merged
    })
}

/// Turn same-day re-sends into corrections: a report row on the same
/// Europe/Stockholm date as an *earlier* report row of the same company gets
/// `is_correction = true` and `report_kind = NULL`. The earliest timestamp
/// stays the release the market reacted to; [`link_corrections`] then points
/// it at the later row. Idempotent.
pub async fn flag_resends(pool: &PgPool, ticker: &str, mic: &str) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE report_releases r SET is_correction = true, report_kind = NULL \
         WHERE r.ticker = $1 AND r.exchange_mic = $2 AND r.occurred_at IS NOT NULL \
           AND r.report_kind IS NOT NULL AND NOT r.is_correction \
           AND EXISTS ( \
               SELECT 1 FROM report_releases e \
               WHERE e.ticker = r.ticker AND e.exchange_mic = r.exchange_mic \
                 AND e.report_kind IS NOT NULL AND NOT e.is_correction \
                 AND e.occurred_at < r.occurred_at \
                 AND (e.occurred_at AT TIME ZONE 'Europe/Stockholm')::date \
                     = (r.occurred_at AT TIME ZONE 'Europe/Stockholm')::date)",
    )
    .bind(ticker)
    .bind(mic)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
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
              AND c2.occurred_at > o2.occurred_at \
              AND c2.occurred_at <= o2.occurred_at + interval '7 days' \
             WHERE o2.ticker = $1 AND o2.exchange_mic = $2 \
               AND o2.report_kind IS NOT NULL AND NOT o2.is_correction \
             ORDER BY o2.id, c2.occurred_at DESC \
         ) c \
         WHERE o.id = c.original_id AND o.superseded_by IS DISTINCT FROM c.id",
    )
    .bind(ticker)
    .bind(mic)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
