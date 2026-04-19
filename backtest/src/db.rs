use chrono::NaiveDate;
use sqlx::PgPool;

pub struct TradeResult {
    pub ticker: String,
    pub earnings_date: NaiveDate,
    pub buy_date: NaiveDate,
    pub buy_price: f64,
    pub sell_price: f64,
    pub pnl: f64,
    pub pnl_pct: f64,
}

pub async fn insert_trade_result(pool: &PgPool, r: &TradeResult) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO trade_results (ticker, earnings_date, buy_date, buy_price, sell_price, pnl, pnl_pct)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(&r.ticker)
    .bind(r.earnings_date)
    .bind(r.buy_date)
    .bind(r.buy_price)
    .bind(r.sell_price)
    .bind(r.pnl)
    .bind(r.pnl_pct)
    .execute(pool)
    .await?;
    Ok(())
}
