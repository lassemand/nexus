use chrono::NaiveDate;
use model::sector::Sector;
use sqlx::PgPool;

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
