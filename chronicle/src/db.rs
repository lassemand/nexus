use sqlx::PgPool;

pub async fn is_published(pool: &PgPool, accession_number: &str) -> sqlx::Result<bool> {
    let row = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM published_filings WHERE accession_number = $1)",
    )
    .bind(accession_number)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn mark_published(
    pool: &PgPool,
    accession_number: &str,
    ticker: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO published_filings (accession_number, ticker) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(accession_number)
    .bind(ticker)
    .execute(pool)
    .await?;
    Ok(())
}
