//! Official daily bars (`market.daily_bars` → `daily_bars`, NEX-162).
//!
//! saxo-stream publishes one [`DailyBar`] per instrument and trading day, with
//! prices as decimal strings. They are validated with [`Decimal`] and written
//! as text cast to `NUMERIC`, so no digit ever passes through a float. A later
//! message for the same `(ticker, exchange_mic, date)` overwrites the stored
//! one (Saxo corrections).

use chrono::{DateTime, NaiveDate, Utc};
use model::generated::DailyBar;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::str::FromStr;
use thiserror::Error;

/// Prices must fit `NUMERIC(20,6)`, i.e. be below 10^14.
const MAX_PRICE: Decimal = Decimal::from_parts(276_447_232, 23_283, 0, false, 0); // 1e14

/// Why a message was rejected. Logged and skipped: a redelivery would be
/// rejected the same way.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Invalid {
    #[error("empty ticker")]
    NoTicker,
    #[error("empty exchange_mic")]
    NoExchange,
    #[error("invalid date {0:?}")]
    Date(String),
    #[error("invalid {field} {value:?}")]
    Price { field: &'static str, value: String },
    #[error("volume {0} does not fit a BIGINT")]
    Volume(u64),
    #[error("invalid fetched_at {0}")]
    FetchedAt(i64),
}

/// A validated bar, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub ticker: String,
    pub exchange_mic: String,
    pub date: NaiveDate,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: i64,
    pub currency: String,
    pub source: String,
    pub fetched_at: DateTime<Utc>,
}

fn price(field: &'static str, value: &str) -> Result<Decimal, Invalid> {
    let bad = || Invalid::Price {
        field,
        value: value.to_string(),
    };
    let d = Decimal::from_str(value.trim()).map_err(|_| bad())?;
    if d.is_sign_negative() || d >= MAX_PRICE {
        return Err(bad());
    }
    Ok(d)
}

/// Validates a message.
pub fn parse(bar: &DailyBar) -> Result<Parsed, Invalid> {
    if bar.ticker.is_empty() {
        return Err(Invalid::NoTicker);
    }
    if bar.exchange_mic.is_empty() {
        return Err(Invalid::NoExchange);
    }
    Ok(Parsed {
        ticker: bar.ticker.clone(),
        exchange_mic: bar.exchange_mic.clone(),
        date: NaiveDate::parse_from_str(&bar.date, "%Y-%m-%d")
            .map_err(|_| Invalid::Date(bar.date.clone()))?,
        open: price("open", &bar.open)?,
        high: price("high", &bar.high)?,
        low: price("low", &bar.low)?,
        close: price("close", &bar.close)?,
        volume: i64::try_from(bar.volume).map_err(|_| Invalid::Volume(bar.volume))?,
        currency: bar.currency.clone(),
        source: bar.source.clone(),
        fetched_at: DateTime::from_timestamp(bar.fetched_at_unix_secs, 0)
            .ok_or(Invalid::FetchedAt(bar.fetched_at_unix_secs))?,
    })
}

/// Inserts the bar, or overwrites the stored one for the same day.
pub async fn store(pool: &PgPool, bar: &Parsed) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO daily_bars
            (ticker, exchange_mic, date, open, high, low, close, volume, currency, source, fetched_at)
        VALUES ($1, $2, $3, $4::numeric, $5::numeric, $6::numeric, $7::numeric, $8, $9, $10, $11)
        ON CONFLICT (ticker, exchange_mic, date) DO UPDATE SET
            open       = EXCLUDED.open,
            high       = EXCLUDED.high,
            low        = EXCLUDED.low,
            close      = EXCLUDED.close,
            volume     = EXCLUDED.volume,
            currency   = EXCLUDED.currency,
            source     = EXCLUDED.source,
            fetched_at = EXCLUDED.fetched_at
        "#,
    )
    .bind(&bar.ticker)
    .bind(&bar.exchange_mic)
    .bind(bar.date)
    .bind(bar.open.to_string())
    .bind(bar.high.to_string())
    .bind(bar.low.to_string())
    .bind(bar.close.to_string())
    .bind(bar.volume)
    .bind(&bar.currency)
    .bind(&bar.source)
    .bind(bar.fetched_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Hook called after bars for an instrument were stored.
///
/// NEX-163 replaces the body with `refresh_report_reactions` for this
/// instrument; that function does not exist yet, so this is a deliberate no-op.
pub async fn after_bars_stored(_pool: &PgPool, _ticker: &str, _exchange_mic: &str) {
    // TODO(NEX-163): call refresh_report_reactions(pool, ticker, exchange_mic).
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    fn message(date: &str, close: &str) -> DailyBar {
        DailyBar {
            ticker: "GOMX".into(),
            exchange_mic: "FNSE".into(),
            date: date.into(),
            open: "12.000001".into(),
            high: "13.5".into(),
            low: "11.999999".into(),
            close: close.into(),
            volume: 1_234_567,
            currency: "SEK".into(),
            source: "saxo".into(),
            fetched_at_unix_secs: 1_790_000_000,
        }
    }

    #[test]
    fn max_price_constant_is_1e14() {
        assert_eq!(MAX_PRICE, Decimal::from_str("100000000000000").unwrap());
    }

    #[test]
    fn valid_message_parses_without_losing_digits() {
        let p = parse(&message("2026-08-26", "12.345678")).unwrap();
        assert_eq!(p.close.to_string(), "12.345678");
        assert_eq!(p.open.to_string(), "12.000001");
        assert_eq!(p.date, "2026-08-26".parse().unwrap());
        assert_eq!(p.volume, 1_234_567);
    }

    #[test]
    fn malformed_messages_are_rejected() {
        let mut m = message("2026-08-26", "12.3");
        m.ticker.clear();
        assert_eq!(parse(&m), Err(Invalid::NoTicker));
        let mut m = message("2026-08-26", "12.3");
        m.exchange_mic.clear();
        assert_eq!(parse(&m), Err(Invalid::NoExchange));
        assert!(matches!(
            parse(&message("26/08/2026", "1")),
            Err(Invalid::Date(_))
        ));
        for bad in ["abc", "", "-1.0", "NaN", "1e20", "100000000000000"] {
            assert!(
                matches!(
                    parse(&message("2026-08-26", bad)),
                    Err(Invalid::Price { field: "close", .. })
                ),
                "{bad}"
            );
        }
        let mut m = message("2026-08-26", "1");
        m.volume = u64::MAX;
        assert_eq!(parse(&m), Err(Invalid::Volume(u64::MAX)));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn numeric_values_are_stored_exactly(pool: PgPool) {
        let p = parse(&message("2026-08-26", "12.345678")).unwrap();
        store(&pool, &p).await.unwrap();
        let row = sqlx::query(
            "SELECT open::text AS o, high::text AS h, low::text AS l, close::text AS c, volume, \
                    currency, source FROM daily_bars",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("o"), "12.000001");
        assert_eq!(row.get::<String, _>("h"), "13.500000");
        assert_eq!(row.get::<String, _>("l"), "11.999999");
        assert_eq!(row.get::<String, _>("c"), "12.345678");
        assert_eq!(row.get::<i64, _>("volume"), 1_234_567);
        assert_eq!(row.get::<String, _>("currency"), "SEK");
        assert_eq!(row.get::<String, _>("source"), "saxo");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_second_message_for_the_same_day_overwrites(pool: PgPool) {
        store(&pool, &parse(&message("2026-08-26", "12.345678")).unwrap())
            .await
            .unwrap();
        let mut corrected = message("2026-08-26", "12.400000");
        corrected.volume = 99;
        store(&pool, &parse(&corrected).unwrap()).await.unwrap();
        // Another day and another exchange are separate rows.
        store(&pool, &parse(&message("2026-08-27", "13.0")).unwrap())
            .await
            .unwrap();
        let mut other = message("2026-08-26", "1.0");
        other.exchange_mic = "XSTO".into();
        store(&pool, &parse(&other).unwrap()).await.unwrap();

        let rows = sqlx::query(
            "SELECT exchange_mic, date::text AS d, close::text AS c, volume FROM daily_bars \
             ORDER BY exchange_mic, date",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let got: Vec<(String, String, String, i64)> = rows
            .iter()
            .map(|r| {
                (
                    r.get("exchange_mic"),
                    r.get("d"),
                    r.get("c"),
                    r.get("volume"),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("FNSE".into(), "2026-08-26".into(), "12.400000".into(), 99),
                (
                    "FNSE".into(),
                    "2026-08-27".into(),
                    "13.000000".into(),
                    1_234_567
                ),
                (
                    "XSTO".into(),
                    "2026-08-26".into(),
                    "1.000000".into(),
                    1_234_567
                ),
            ]
        );
    }
}
