use chrono::{DateTime, NaiveDate, Utc};
use model::sector::Sector;
use sqlx::PgPool;

use crate::features::{Bar, PreEventFeatures};

pub struct UnlabeledEvent {
    pub id: i64,
    pub event_date: NaiveDate,
}

pub struct TradeResult {
    pub ticker: String,
    pub earnings_date: NaiveDate,
    pub buy_date: NaiveDate,
    pub sell_date: NaiveDate,
    pub buy_price: f64,
    pub sell_price: f64,
    pub pnl: f64,
    pub pnl_pct: f64,
}

/// Persists a completed trade simulation result.
pub async fn insert_trade_result(pool: &PgPool, r: &TradeResult) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO trade_results (ticker, earnings_date, buy_date, sell_date, buy_price, sell_price, pnl, pnl_pct)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
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
/// `ticker` is normalized to uppercase before being stored. On conflict the
/// sector is overwritten with the new value.
pub async fn upsert_company(pool: &PgPool, ticker: &str, sector: Sector) -> sqlx::Result<()> {
    let ticker = ticker.to_uppercase();
    sqlx::query(
        r#"
        INSERT INTO companies (ticker, sector)
        VALUES ($1, $2)
        ON CONFLICT (ticker) DO UPDATE SET sector = EXCLUDED.sector
        "#,
    )
    .bind(&ticker)
    .bind(sector.slug())
    .execute(pool)
    .await?;
    Ok(())
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
        upsert_company(&pool, "AAPL", Sector::Technology).await?;
        let map = load_companies(&pool).await?;
        assert_eq!(map.get("AAPL"), Some(&Sector::Technology));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn test_upsert_updates_sector_on_conflict(pool: PgPool) -> sqlx::Result<()> {
        upsert_company(&pool, "AAPL", Sector::Technology).await?;
        upsert_company(&pool, "AAPL", Sector::Healthcare).await?;
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
            upsert_company(&pool, ticker, sector.clone()).await?;
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
        sqlx::query("INSERT INTO companies (ticker, sector) VALUES ($1, $2)")
            .bind("BAD")
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
        upsert_company(&pool, "aapl", Sector::Technology).await?;
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
        upsert_company(&pool, "AAPL", Sector::Technology).await?;
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
        upsert_company(&pool, "aapl", Sector::Technology).await?; // stored as AAPL
        assert!(is_registered(&pool, "AAPL").await?, "uppercase must match");
        assert!(
            !is_registered(&pool, "aapl").await?,
            "lowercase must not match after normalisation"
        );
        Ok(())
    }
}
