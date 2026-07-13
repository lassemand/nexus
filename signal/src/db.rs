use alpha::country_for_exchange;
use chrono::{DateTime, NaiveDate, Utc};
use model::{calendar::DynamicCalendar, generated::InsiderFiling, sector::Sector};
use sqlx::PgPool;
use std::collections::HashSet;

use crate::features::{Bar, PreEventFeatures};

pub struct UnlabeledEvent {
    pub id: i64,
    pub event_date: NaiveDate,
}

#[derive(Debug)]
pub struct TradeResult {
    pub ticker: String,
    pub earnings_date: NaiveDate,
    pub buy_date: NaiveDate,
    pub sell_date: NaiveDate,
    pub buy_price: f64,
    pub sell_price: f64,
    pub pnl: f64,
    pub pnl_pct: f64,
    /// ISO 4217 currency code for all price fields in this result (e.g. "USD", "SEK").
    /// Both buy_price and sell_price are always in this currency — cross-currency
    /// trades are rejected at the strategy layer before reaching here.
    pub currency: String,
}

/// Persists a completed trade simulation result.
pub async fn insert_trade_result(pool: &PgPool, r: &TradeResult) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO trade_results (ticker, earnings_date, buy_date, sell_date, buy_price, sell_price, pnl, pnl_pct, currency)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(&r.ticker)
    .bind(r.earnings_date)
    .bind(r.buy_date)
    .bind(r.sell_date)
    .bind(r.buy_price)
    .bind(r.sell_price)
    .bind(r.pnl)
    .bind(r.pnl_pct)
    .bind(&r.currency)
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts a special event, ignoring duplicates (idempotent under at-least-once delivery).
///
/// `description` is stored as `NULL` when the proto field is an empty string.
pub async fn upsert_special_event(
    pool: &PgPool,
    ticker: &str,
    event_type: &str,
    occurred_at: DateTime<Utc>,
    description: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO special_events (ticker, event_type, occurred_at, description)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (ticker, event_type, occurred_at) DO NOTHING
        "#,
    )
    .bind(ticker)
    .bind(event_type)
    .bind(occurred_at)
    .bind(description)
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts or updates an EOD bar. On conflict (same ticker + date) all OHLCV
/// fields are overwritten, allowing the market binary to re-publish without
/// creating duplicates.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_bar(
    pool: &PgPool,
    ticker: &str,
    date: NaiveDate,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO bars (ticker, date, open, high, low, close, volume)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (ticker, date) DO UPDATE SET
            open   = EXCLUDED.open,
            high   = EXCLUDED.high,
            low    = EXCLUDED.low,
            close  = EXCLUDED.close,
            volume = EXCLUDED.volume
        "#,
    )
    .bind(ticker)
    .bind(date)
    .bind(open)
    .bind(high)
    .bind(low)
    .bind(close)
    .bind(volume)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetches up to `limit` bars before `before_date` for `ticker`, returned in
/// chronological order (ascending date).
pub async fn fetch_bars_before(
    pool: &PgPool,
    ticker: &str,
    before_date: NaiveDate,
    limit: i64,
) -> sqlx::Result<Vec<Bar>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT date, close, volume FROM bars \
         WHERE ticker = $1 AND date < $2 \
         ORDER BY date DESC LIMIT $3",
    )
    .bind(ticker)
    .bind(before_date)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut bars: Vec<Bar> = rows
        .into_iter()
        .map(|r| Bar {
            close: r.get("close"),
            volume: r.get("volume"),
        })
        .collect();
    bars.reverse();
    Ok(bars)
}

/// Inserts an event signal row. Ignores duplicates (idempotent).
pub async fn insert_event_signal(
    pool: &PgPool,
    ticker: &str,
    event_type: &str,
    event_date: NaiveDate,
    f: &PreEventFeatures,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO event_signals (
            ticker, event_type, event_date,
            car_20d, car_10d, car_5d,
            mean_abvol_20d, max_abvol_20d, realized_vol_20d,
            price_momentum_20d, vol_trend_slope, pre_event_bars
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (ticker, event_type, event_date) DO NOTHING
        "#,
    )
    .bind(ticker)
    .bind(event_type)
    .bind(event_date)
    .bind(f.car_20d)
    .bind(f.car_10d)
    .bind(f.car_5d)
    .bind(f.mean_abvol_20d)
    .bind(f.max_abvol_20d)
    .bind(f.realized_vol_20d)
    .bind(f.price_momentum_20d)
    .bind(f.vol_trend_slope)
    .bind(f.pre_event_bars)
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns unlabeled event_signals rows for `ticker` whose `event_date` falls
/// within 8 calendar days before `bar_date` (≈ 5 trading days, accounting for
/// weekends). These events now have post-event bars available for truth labeling.
pub async fn fetch_unlabeled_events_needing_bar(
    pool: &PgPool,
    ticker: &str,
    bar_date: NaiveDate,
) -> sqlx::Result<Vec<UnlabeledEvent>> {
    use sqlx::Row;
    let rows = sqlx::query(
        r#"
        SELECT id, event_date FROM event_signals
        WHERE ticker = $1
          AND truth_labeled_at IS NULL
          AND event_date <= $2
          AND event_date >= $2 - INTERVAL '8 days'
        ORDER BY event_date
        "#,
    )
    .bind(ticker)
    .bind(bar_date)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| UnlabeledEvent {
            id: r.get("id"),
            event_date: r.get("event_date"),
        })
        .collect())
}

/// Fills in the post-event truth columns on an event_signals row.
pub async fn label_event_truth(
    pool: &PgPool,
    id: i64,
    post_ar_1d: f64,
    post_ar_5d: f64,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        UPDATE event_signals
        SET post_ar_1d = $1, post_ar_5d = $2, truth_labeled_at = NOW()
        WHERE id = $3
        "#,
    )
    .bind(post_ar_1d)
    .bind(post_ar_5d)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetches up to `limit` bars strictly after `after_date` for `ticker`,
/// returned in chronological order (ascending date).
pub async fn fetch_bars_after(
    pool: &PgPool,
    ticker: &str,
    after_date: NaiveDate,
    limit: i64,
) -> sqlx::Result<Vec<Bar>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT close, volume FROM bars \
         WHERE ticker = $1 AND date > $2 \
         ORDER BY date ASC LIMIT $3",
    )
    .bind(ticker)
    .bind(after_date)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| Bar {
            close: r.get("close"),
            volume: r.get("volume"),
        })
        .collect())
}

/// Upserts a person record and inserts the filing, all in one transaction.
///
/// - If `filer_cik` is non-empty: upserts on the partial unique index, updating the name.
/// - If `filer_cik` is empty: always inserts a new person row (nullable CIK).
/// - Filing insert is `ON CONFLICT DO NOTHING` (idempotent).
pub async fn upsert_insider_filing(pool: &PgPool, f: &InsiderFiling) -> sqlx::Result<()> {
    use sqlx::Row;
    let transaction_date = DateTime::from_timestamp(f.transaction_date_unix_secs, 0)
        .map(|dt| dt.date_naive())
        .unwrap_or_default();
    let filing_date = DateTime::from_timestamp(f.filing_date_unix_secs, 0)
        .map(|dt| dt.date_naive())
        .unwrap_or_default();

    let mut tx = pool.begin().await?;

    let person_id: i64 = if f.filer_cik.is_empty() {
        sqlx::query("INSERT INTO persons (filer_cik, filer_name) VALUES (NULL, $1) RETURNING id")
            .bind(&f.filer_name)
            .fetch_one(&mut *tx)
            .await?
            .get("id")
    } else {
        sqlx::query(
            r#"
            INSERT INTO persons (filer_cik, filer_name)
            VALUES ($1, $2)
            ON CONFLICT (filer_cik) WHERE filer_cik IS NOT NULL
            DO UPDATE SET filer_name = EXCLUDED.filer_name, updated_at = NOW()
            RETURNING id
            "#,
        )
        .bind(&f.filer_cik)
        .bind(&f.filer_name)
        .fetch_one(&mut *tx)
        .await?
        .get("id")
    };

    sqlx::query(
        r#"
        INSERT INTO insider_filings
            (person_id, ticker, issuer_cik, filer_role,
             transaction_date, filing_date, shares, price_per_share, transaction_code)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (person_id, ticker, transaction_date, shares, transaction_code) DO NOTHING
        "#,
    )
    .bind(person_id)
    .bind(&f.ticker)
    .bind(&f.issuer_cik)
    .bind(&f.filer_role)
    .bind(transaction_date)
    .bind(filing_date)
    .bind(f.shares)
    .bind(f.price_per_share)
    .bind(&f.transaction_code)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Returns tickers with at least one insider purchase (`P`) on or after `since`,
/// along with the person_ids of those buyers. Used by the prospective scanner (NEX-55).
#[allow(dead_code)]
pub async fn fetch_recent_insider_activity(
    pool: &PgPool,
    since: NaiveDate,
) -> sqlx::Result<Vec<(String, Vec<i64>)>> {
    use sqlx::Row;
    let rows = sqlx::query(
        r#"
        SELECT ticker,
               array_agg(DISTINCT person_id ORDER BY person_id) AS person_ids
        FROM insider_filings
        WHERE transaction_date >= $1
          AND transaction_code = 'P'
        GROUP BY ticker
        "#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| (r.get("ticker"), r.get("person_ids")))
        .collect())
}

/// Returns `true` if the ticker exists in the `companies` table.
///
/// The comparison is case-sensitive; callers should normalise to uppercase
/// before calling.
pub async fn is_registered(pool: &PgPool, ticker: &str) -> sqlx::Result<bool> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM companies WHERE ticker = $1)")
            .bind(ticker)
            .fetch_one(pool)
            .await?;
    Ok(exists)
}

/// Inserts or updates the sector mapping for a company.
///
/// Upsert a company into the registry.
///
/// `ticker` is normalized to uppercase. On conflict (ticker, exchange_mic) the
/// sector is updated; exchange_mic and currency are immutable after first insert.
pub async fn upsert_company(
    pool: &PgPool,
    ticker: &str,
    exchange_mic: &str,
    currency: &str,
    sector: Sector,
) -> sqlx::Result<()> {
    let ticker = ticker.to_uppercase();
    sqlx::query(
        r#"
        INSERT INTO companies (ticker, exchange_mic, currency, sector)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (ticker, exchange_mic) DO UPDATE SET sector = EXCLUDED.sector
        "#,
    )
    .bind(&ticker)
    .bind(exchange_mic)
    .bind(currency)
    .bind(sector.slug())
    .execute(pool)
    .await?;
    Ok(())
}

/// Load a [`DynamicCalendar`] for `exchange_mic` from the `trading_holidays` table.
///
/// Resolves the exchange MIC to a country code via [`alpha::country_for_exchange`],
/// then fetches all rows for that country. Returns an empty calendar with a
/// warning if no rows exist.
// Used by the upcoming Nordic bar-gap validation task.
#[allow(dead_code)]
pub async fn load_trading_calendar(
    pool: &PgPool,
    exchange_mic: &str,
) -> sqlx::Result<DynamicCalendar> {
    use sqlx::Row;

    let country = match country_for_exchange(exchange_mic) {
        Some(c) => c,
        None => {
            tracing::warn!(
                exchange_mic,
                "no country mapping found for exchange, returning empty calendar"
            );
            return Ok(DynamicCalendar::new(Default::default(), Default::default()));
        }
    };

    let rows = sqlx::query("SELECT date, status FROM trading_holidays WHERE country = $1")
        .bind(country)
        .fetch_all(pool)
        .await?;

    if rows.is_empty() {
        tracing::warn!(
            exchange_mic,
            country,
            "no trading_holidays rows found for country, returning empty calendar"
        );
    }

    let mut closed = HashSet::new();
    let mut half_days = HashSet::new();

    for row in rows {
        let date: NaiveDate = row.get("date");
        let status: &str = row.get("status");
        match status {
            "closed" => {
                closed.insert(date);
            }
            "half_day" => {
                half_days.insert(date);
            }
            other => {
                tracing::warn!(
                    exchange_mic,
                    country,
                    %date,
                    status = other,
                    "unknown trading_holidays status, skipping"
                );
            }
        }
    }

    Ok(DynamicCalendar::new(closed, half_days))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use std::collections::HashMap;
    use tracing::warn;

    /// Maps a GICS sector slug string back to a [`Sector`] variant.
    /// Returns `None` for any unrecognized slug.
    fn sector_from_slug(slug: &str) -> Option<Sector> {
        match slug {
            "technology" => Some(Sector::Technology),
            "healthcare" => Some(Sector::Healthcare),
            "financials" => Some(Sector::Financials),
            "consumer-discretionary" => Some(Sector::ConsumerDiscretionary),
            "consumer-staples" => Some(Sector::ConsumerStaples),
            "energy" => Some(Sector::Energy),
            "utilities" => Some(Sector::Utilities),
            "industrials" => Some(Sector::Industrials),
            "materials" => Some(Sector::Materials),
            "real-estate" => Some(Sector::RealEstate),
            "communication-services" => Some(Sector::CommunicationServices),
            _ => None,
        }
    }

    /// Loads all company sector mappings from the database.
    ///
    /// Used only in tests to assert the state persisted by `upsert_company`.
    async fn load_companies(pool: &PgPool) -> sqlx::Result<HashMap<String, Sector>> {
        let rows = sqlx::query("SELECT ticker, sector FROM companies")
            .fetch_all(pool)
            .await?;

        let mut map = HashMap::new();
        for row in rows {
            let ticker: String = row.get("ticker");
            let slug: String = row.get("sector");
            match sector_from_slug(&slug) {
                Some(sector) => {
                    map.insert(ticker, sector);
                }
                None => {
                    warn!(ticker = %ticker, slug = %slug, "unrecognized sector slug in companies table, skipping row");
                }
            }
        }
        Ok(map)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_upsert_inserts_new_row(pool: PgPool) -> sqlx::Result<()> {
        upsert_company(&pool, "AAPL", "XNAS", "USD", Sector::Technology).await?;
        let map = load_companies(&pool).await?;
        assert_eq!(map.get("AAPL"), Some(&Sector::Technology));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_upsert_updates_sector_on_conflict(pool: PgPool) -> sqlx::Result<()> {
        upsert_company(&pool, "AAPL", "XNAS", "USD", Sector::Technology).await?;
        upsert_company(&pool, "AAPL", "XNAS", "USD", Sector::Healthcare).await?;
        let map = load_companies(&pool).await?;
        assert_eq!(map.get("AAPL"), Some(&Sector::Healthcare));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_load_companies_empty_table(pool: PgPool) -> sqlx::Result<()> {
        let map = load_companies(&pool).await?;
        assert!(map.is_empty());
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_load_companies_all_eleven_sectors(pool: PgPool) -> sqlx::Result<()> {
        let fixtures = [
            ("T1", Sector::Technology),
            ("T2", Sector::Healthcare),
            ("T3", Sector::Financials),
            ("T4", Sector::ConsumerDiscretionary),
            ("T5", Sector::ConsumerStaples),
            ("T6", Sector::Energy),
            ("T7", Sector::Utilities),
            ("T8", Sector::Industrials),
            ("T9", Sector::Materials),
            ("T10", Sector::RealEstate),
            ("T11", Sector::CommunicationServices),
        ];
        for (ticker, ref sector) in &fixtures {
            upsert_company(&pool, ticker, "XNAS", "USD", sector.clone()).await?;
        }
        let map = load_companies(&pool).await?;
        assert_eq!(map.len(), 11);
        for (ticker, sector) in &fixtures {
            assert_eq!(map.get(*ticker), Some(sector), "wrong sector for {ticker}");
        }
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_unknown_slug_is_skipped_not_panicked(pool: PgPool) -> sqlx::Result<()> {
        // Insert a row with an unrecognized slug directly — bypassing upsert_company
        // to simulate schema drift or a future variant not yet known to this binary.
        sqlx::query(
            "INSERT INTO companies (ticker, exchange_mic, currency, sector) VALUES ($1, $2, $3, $4)",
        )
        .bind("BAD")
        .bind("XNAS")
        .bind("USD")
        .bind("unknown_sector")
        .execute(&pool)
        .await?;

        let map = load_companies(&pool).await?;
        assert!(
            !map.contains_key("BAD"),
            "unknown slug must be skipped, not returned"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_ticker_normalization_to_uppercase(pool: PgPool) -> sqlx::Result<()> {
        upsert_company(&pool, "aapl", "XNAS", "USD", Sector::Technology).await?;
        let map = load_companies(&pool).await?;
        assert_eq!(
            map.get("AAPL"),
            Some(&Sector::Technology),
            "ticker must be stored as uppercase"
        );
        assert!(
            !map.contains_key("aapl"),
            "lowercase key must not appear in result"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_is_registered_returns_true_for_known_ticker(pool: PgPool) -> sqlx::Result<()> {
        upsert_company(&pool, "AAPL", "XNAS", "USD", Sector::Technology).await?;
        assert!(is_registered(&pool, "AAPL").await?);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_is_registered_returns_false_for_unknown_ticker(pool: PgPool) -> sqlx::Result<()> {
        assert!(!is_registered(&pool, "ZZZZ").await?);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_is_registered_is_case_sensitive(pool: PgPool) -> sqlx::Result<()> {
        upsert_company(&pool, "aapl", "XNAS", "USD", Sector::Technology).await?; // stored as AAPL
        assert!(is_registered(&pool, "AAPL").await?, "uppercase must match");
        assert!(
            !is_registered(&pool, "aapl").await?,
            "lowercase must not match after normalisation"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_nordic_ticker_stored_with_correct_mic_and_currency(
        pool: PgPool,
    ) -> sqlx::Result<()> {
        use sqlx::Row;

        // Register GOMX as an FNSE instrument with SEK currency.
        upsert_company(&pool, "GOMX", "FNSE", "SEK", Sector::Technology).await?;

        let row = sqlx::query(
            "SELECT ticker, exchange_mic, currency FROM companies WHERE ticker = 'GOMX'",
        )
        .fetch_one(&pool)
        .await?;

        assert_eq!(row.get::<String, _>("ticker"), "GOMX");
        assert_eq!(row.get::<String, _>("exchange_mic"), "FNSE");
        assert_eq!(row.get::<String, _>("currency"), "SEK");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_same_ticker_different_mic_does_not_collide(pool: PgPool) -> sqlx::Result<()> {
        use sqlx::Row;

        // Hypothetical: same ticker string on two different exchanges.
        upsert_company(&pool, "GOMX", "XNAS", "USD", Sector::Technology).await?;
        upsert_company(&pool, "GOMX", "FNSE", "SEK", Sector::Technology).await?;

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM companies WHERE ticker = 'GOMX'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            count, 2,
            "same ticker on different exchanges must be separate rows"
        );

        let usd_row = sqlx::query(
            "SELECT currency FROM companies WHERE ticker = 'GOMX' AND exchange_mic = 'XNAS'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(usd_row.get::<String, _>("currency"), "USD");

        let sek_row = sqlx::query(
            "SELECT currency FROM companies WHERE ticker = 'GOMX' AND exchange_mic = 'FNSE'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(sek_row.get::<String, _>("currency"), "SEK");

        Ok(())
    }
}
