use chrono::NaiveDate;
use sqlx::PgPool;

/// A row in the `earnings_dates` table, used for upsert and query operations.
///
/// `id` and `created_at` are managed by the database and are not included here.
pub struct EarningsRecord {
    pub ticker: String,
    pub fiscal_year: i16,
    pub fiscal_quarter: i16,
    pub period_end: NaiveDate,
    pub filing_date: NaiveDate,
    pub eps_actual: Option<f64>,
    pub revenue_actual: Option<f64>,
}

/// Upserts an earnings record into `earnings_dates`.
///
/// On conflict on `(ticker, fiscal_year, fiscal_quarter)` all mutable fields
/// (`period_end`, `filing_date`, `eps_actual`, `revenue_actual`) are overwritten.
/// `id`, `published_at`, and `created_at` are left unchanged on conflict.
pub async fn upsert_earnings_date(pool: &PgPool, record: &EarningsRecord) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO earnings_dates (ticker, fiscal_year, fiscal_quarter, period_end, filing_date, eps_actual, revenue_actual)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT ON CONSTRAINT uq_ticker_quarter DO UPDATE SET
            period_end     = EXCLUDED.period_end,
            filing_date    = EXCLUDED.filing_date,
            eps_actual     = EXCLUDED.eps_actual,
            revenue_actual = EXCLUDED.revenue_actual
        "#,
    )
    .bind(&record.ticker)
    .bind(record.fiscal_year)
    .bind(record.fiscal_quarter)
    .bind(record.period_end)
    .bind(record.filing_date)
    .bind(record.eps_actual)
    .bind(record.revenue_actual)
    .execute(pool)
    .await?;
    Ok(())
}

/// Sets `published_at = now()` for the row identified by `(ticker, fiscal_year, fiscal_quarter)`.
///
/// This should be called after the record has been successfully published to Kafka.
/// If no matching row exists the call is a no-op.
pub async fn mark_earnings_published(
    pool: &PgPool,
    ticker: &str,
    fiscal_year: i16,
    fiscal_quarter: i16,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE earnings_dates
        SET published_at = now()
        WHERE ticker = $1
          AND fiscal_year = $2
          AND fiscal_quarter = $3
        "#,
    )
    .bind(ticker)
    .bind(fiscal_year)
    .bind(fiscal_quarter)
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns all unpublished earnings records for a ticker, ordered by `period_end ASC`.
///
/// A record is considered unpublished when `published_at IS NULL`.
pub async fn load_unpublished_earnings(
    pool: &PgPool,
    ticker: &str,
) -> Result<Vec<EarningsRecord>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT ticker, fiscal_year, fiscal_quarter, period_end, filing_date, eps_actual, revenue_actual
        FROM earnings_dates
        WHERE ticker = $1
          AND published_at IS NULL
        ORDER BY period_end ASC
        "#,
    )
    .bind(ticker)
    .fetch_all(pool)
    .await?;

    let records = rows
        .into_iter()
        .map(|row| {
            use sqlx::Row;
            EarningsRecord {
                ticker: row.get("ticker"),
                fiscal_year: row.get("fiscal_year"),
                fiscal_quarter: row.get("fiscal_quarter"),
                period_end: row.get("period_end"),
                filing_date: row.get("filing_date"),
                eps_actual: row.get("eps_actual"),
                revenue_actual: row.get("revenue_actual"),
            }
        })
        .collect();

    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn make_record(
        ticker: &str,
        fiscal_year: i16,
        fiscal_quarter: i16,
        period_end: NaiveDate,
        filing_date: NaiveDate,
        eps_actual: Option<f64>,
        revenue_actual: Option<f64>,
    ) -> EarningsRecord {
        EarningsRecord {
            ticker: ticker.to_string(),
            fiscal_year,
            fiscal_quarter,
            period_end,
            filing_date,
            eps_actual,
            revenue_actual,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_insert_new_row_succeeds(pool: PgPool) -> sqlx::Result<()> {
        let record = make_record(
            "AAPL",
            2025,
            1,
            NaiveDate::from_ymd_opt(2025, 3, 31).unwrap(),
            NaiveDate::from_ymd_opt(2025, 4, 15).unwrap(),
            Some(1.52),
            Some(90_000.0),
        );
        upsert_earnings_date(&pool, &record).await?;

        let rows = load_unpublished_earnings(&pool, "AAPL").await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ticker, "AAPL");
        assert_eq!(rows[0].fiscal_year, 2025);
        assert_eq!(rows[0].fiscal_quarter, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_upsert_overwrites_on_conflict(pool: PgPool) -> sqlx::Result<()> {
        let original = make_record(
            "MSFT",
            2025,
            2,
            NaiveDate::from_ymd_opt(2025, 6, 30).unwrap(),
            NaiveDate::from_ymd_opt(2025, 7, 10).unwrap(),
            Some(2.00),
            Some(50_000.0),
        );
        upsert_earnings_date(&pool, &original).await?;

        let updated = make_record(
            "MSFT",
            2025,
            2,
            NaiveDate::from_ymd_opt(2025, 6, 30).unwrap(),
            NaiveDate::from_ymd_opt(2025, 7, 25).unwrap(), // changed
            Some(2.95),                                     // changed
            Some(55_000.0),                                 // changed
        );
        upsert_earnings_date(&pool, &updated).await?;

        let rows = load_unpublished_earnings(&pool, "MSFT").await?;
        assert_eq!(rows.len(), 1, "must still be exactly one row after upsert");
        assert_eq!(rows[0].filing_date, NaiveDate::from_ymd_opt(2025, 7, 25).unwrap());
        assert_eq!(rows[0].eps_actual, Some(2.95));
        assert_eq!(rows[0].revenue_actual, Some(55_000.0));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_mark_earnings_published_sets_published_at(pool: PgPool) -> sqlx::Result<()> {
        let record = make_record(
            "GOOG",
            2025,
            3,
            NaiveDate::from_ymd_opt(2025, 9, 30).unwrap(),
            NaiveDate::from_ymd_opt(2025, 10, 5).unwrap(),
            None,
            None,
        );
        upsert_earnings_date(&pool, &record).await?;

        // Before marking, should appear in unpublished list
        let before = load_unpublished_earnings(&pool, "GOOG").await?;
        assert_eq!(before.len(), 1);

        mark_earnings_published(&pool, "GOOG", 2025, 3).await?;

        // After marking, should no longer appear in unpublished list
        let after = load_unpublished_earnings(&pool, "GOOG").await?;
        assert_eq!(after.len(), 0, "published row must not appear in unpublished results");

        // Verify published_at is actually set in the database
        let published_at: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT published_at FROM earnings_dates WHERE ticker = $1 AND fiscal_year = $2 AND fiscal_quarter = $3",
        )
        .bind("GOOG")
        .bind(2025_i16)
        .bind(3_i16)
        .fetch_one(&pool)
        .await?;
        assert!(published_at.is_some(), "published_at must be non-null after marking");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_load_unpublished_excludes_published_rows(pool: PgPool) -> sqlx::Result<()> {
        // Insert two records for the same ticker
        let q1 = make_record(
            "AMZN",
            2025,
            1,
            NaiveDate::from_ymd_opt(2025, 3, 31).unwrap(),
            NaiveDate::from_ymd_opt(2025, 4, 20).unwrap(),
            Some(0.80),
            Some(120_000.0),
        );
        let q2 = make_record(
            "AMZN",
            2025,
            2,
            NaiveDate::from_ymd_opt(2025, 6, 30).unwrap(),
            NaiveDate::from_ymd_opt(2025, 7, 20).unwrap(),
            Some(0.95),
            Some(130_000.0),
        );
        upsert_earnings_date(&pool, &q1).await?;
        upsert_earnings_date(&pool, &q2).await?;

        // Mark Q1 as published
        mark_earnings_published(&pool, "AMZN", 2025, 1).await?;

        let unpublished = load_unpublished_earnings(&pool, "AMZN").await?;
        assert_eq!(unpublished.len(), 1, "only the unpublished quarter must be returned");
        assert_eq!(unpublished[0].fiscal_quarter, 2);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_load_unpublished_returns_ascending_period_end_order(pool: PgPool) -> sqlx::Result<()> {
        // Insert Q3, Q1, Q2 out of order
        let quarters = [
            (3_i16, NaiveDate::from_ymd_opt(2025, 9, 30).unwrap()),
            (1_i16, NaiveDate::from_ymd_opt(2025, 3, 31).unwrap()),
            (2_i16, NaiveDate::from_ymd_opt(2025, 6, 30).unwrap()),
        ];
        for (q, period_end) in &quarters {
            let record = make_record(
                "META",
                2025,
                *q,
                *period_end,
                NaiveDate::from_ymd_opt(2025, 10, 1).unwrap(),
                None,
                None,
            );
            upsert_earnings_date(&pool, &record).await?;
        }

        let rows = load_unpublished_earnings(&pool, "META").await?;
        assert_eq!(rows.len(), 3);
        // Must be sorted ASC by period_end: Q1, Q2, Q3
        assert_eq!(rows[0].fiscal_quarter, 1);
        assert_eq!(rows[1].fiscal_quarter, 2);
        assert_eq!(rows[2].fiscal_quarter, 3);
        // Verify dates are strictly ascending
        assert!(rows[0].period_end < rows[1].period_end);
        assert!(rows[1].period_end < rows[2].period_end);
        Ok(())
    }
}
