//! The constraints the analyst's schema enforces by itself.

use sqlx::PgPool;

async fn target(pool: &PgPool, ticker: &str, date: &str, status: &str) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO report_targets (ticker, exchange_mic, report_kind, fiscal_period, expected_date, \
             expected_time_known, expected_at, signal_event_id, status, prep_due_at, created_at, updated_at) \
         VALUES ($1, 'XSTO', 'interim', NULL, $2::date, true, now(), floor(random() * 1e9)::bigint, $3, now(), now(), now()) \
         RETURNING id",
    )
    .bind(ticker)
    .bind(date)
    .bind(status)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

async fn kernel(
    pool: &PgPool,
    target_id: i64,
    version: i32,
    status: &str,
) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO kernels (target_id, version, source, wasm, sha256, status, created_at) \
         VALUES ($1, $2, 'src', '\\x00'::bytea, 'abc', $3, now()) RETURNING id",
    )
    .bind(target_id)
    .bind(version)
    .bind(status)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

fn constraint(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|db| db.constraint())
        .map(str::to_owned)
}

#[sqlx::test]
async fn at_most_one_accepted_kernel_per_target(pool: PgPool) {
    let id = target(&pool, "AAA", "2026-12-10", "ready")
        .await
        .expect("target");
    kernel(&pool, id, 1, "accepted")
        .await
        .expect("first accepted");
    let second = kernel(&pool, id, 2, "accepted")
        .await
        .expect_err("a second accepted kernel");
    assert_eq!(
        constraint(&second).as_deref(),
        Some("kernels_one_accepted_idx")
    );
    kernel(&pool, id, 2, "candidate")
        .await
        .expect("a candidate is fine");
    kernel(&pool, id, 3, "superseded")
        .await
        .expect("so is a superseded one");
    let other = target(&pool, "BBB", "2026-12-10", "ready")
        .await
        .expect("target");
    kernel(&pool, other, 1, "accepted")
        .await
        .expect("another target has its own");
}

#[sqlx::test]
async fn versions_are_unique_per_target(pool: PgPool) {
    let id = target(&pool, "AAA", "2026-12-10", "ready")
        .await
        .expect("target");
    kernel(&pool, id, 1, "rejected").await.expect("v1");
    assert!(kernel(&pool, id, 1, "candidate").await.is_err());
}

#[sqlx::test]
async fn kernels_are_never_deleted(pool: PgPool) {
    let id = target(&pool, "AAA", "2026-12-10", "ready")
        .await
        .expect("target");
    let kernel_id = kernel(&pool, id, 1, "spent").await.expect("kernel");
    let error = sqlx::query("DELETE FROM kernels WHERE id = $1")
        .bind(kernel_id)
        .execute(&pool)
        .await
        .expect_err("a kernel cannot be deleted");
    assert!(error.to_string().contains("never deleted"), "{error}");
    let truncated = sqlx::query("DELETE FROM report_targets WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await;
    assert!(
        truncated.is_err(),
        "a target with a kernel cannot be deleted either"
    );
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM kernels")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count.0, 1);
}

#[sqlx::test]
async fn a_report_has_one_live_target(pool: PgPool) {
    target(&pool, "AAA", "2026-12-10", "waiting")
        .await
        .expect("first");
    let duplicate = sqlx::query(
        "INSERT INTO report_targets (ticker, exchange_mic, report_kind, expected_date, expected_time_known, \
             expected_at, signal_event_id, status, prep_due_at, created_at, updated_at) \
         VALUES ('AAA', 'XSTO', 'interim', '2026-12-10', true, now(), 999999999999, 'waiting', now(), now(), now())",
    )
    .execute(&pool)
    .await
    .expect_err("the same report twice");
    assert_eq!(
        constraint(&duplicate).as_deref(),
        Some("report_targets_report_idx")
    );
    sqlx::query("UPDATE report_targets SET status = 'cancelled'")
        .execute(&pool)
        .await
        .expect("cancel");
    target(&pool, "AAA", "2026-12-10", "waiting")
        .await
        .expect("a cancelled one does not count");
}

#[sqlx::test]
async fn bad_values_are_rejected_by_the_database(pool: PgPool) {
    assert!(
        target(&pool, "AAA", "2026-12-10", "sleeping")
            .await
            .is_err(),
        "unknown status"
    );
    let id = target(&pool, "AAA", "2026-12-10", "waiting")
        .await
        .expect("target");
    assert!(
        kernel(&pool, id, 1, "maybe").await.is_err(),
        "unknown kernel status"
    );
    assert!(
        kernel(&pool, id, 0, "candidate").await.is_err(),
        "versions start at 1"
    );
    let long = "x".repeat(4097);
    let note = sqlx::query(
        "INSERT INTO notes (ticker, exchange_mic, text, author, created_at, received_at, source_event_id) \
         VALUES ('AAA', 'XSTO', $1, 'me', now(), now(), gen_random_uuid())",
    )
    .bind(&long)
    .execute(&pool)
    .await;
    assert!(note.is_err(), "a note over 4 KiB");
    let fine = sqlx::query(
        "INSERT INTO notes (ticker, exchange_mic, text, author, created_at, received_at, source_event_id) \
         VALUES ('AAA', 'XSTO', 'ok', 'me', now(), now(), gen_random_uuid())",
    )
    .execute(&pool)
    .await;
    assert!(fine.is_ok());
}
