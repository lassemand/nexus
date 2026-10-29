//! Storing insider transactions without losing any (NEX-207).
//!
//! The consumer used to log a database error and commit the Kafka offset
//! anyway, which dropped the transaction. A transient error (the database is
//! down, the pool is exhausted) is now retried with backoff and the offset is
//! committed only after the row is stored, as for the Nordic report events. An
//! error that cannot go away by waiting (a value the schema rejects) is handed
//! back to the caller, who logs it and moves on: retrying would block the
//! partition forever.

use std::future::Future;
use std::time::Duration;
use tracing::error;

/// True if waiting and trying again can succeed: lost connections, an
/// exhausted pool, serialization failures and server shutdowns. A constraint or
/// data error in the row itself is not.
pub fn is_transient(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => true,
        // 08 connection, 40 serialization / deadlock, 53 resources, 57 operator
        // intervention (shutdown, restart).
        sqlx::Error::Database(db) => db
            .code()
            .is_some_and(|c| ["08", "40", "53", "57"].iter().any(|p| c.starts_with(p))),
        _ => false,
    }
}

/// Runs `op` until it succeeds, sleeping `initial` (doubling up to `max`)
/// between transient failures. Returns the first non-transient error.
pub async fn store_with_retry<F, Fut>(
    what: &str,
    mut op: F,
    initial: Duration,
    max: Duration,
) -> sqlx::Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = sqlx::Result<()>>,
{
    let mut backoff = initial;
    loop {
        match op().await {
            Ok(()) => return Ok(()),
            Err(e) if is_transient(&e) => {
                error!(what, error = %e, retry_in = ?backoff, "failed to store, retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max);
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn io() -> sqlx::Error {
        sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ))
    }

    #[test]
    fn connection_problems_are_transient_and_bad_rows_are_not() {
        assert!(is_transient(&io()));
        assert!(is_transient(&sqlx::Error::PoolTimedOut));
        assert!(is_transient(&sqlx::Error::PoolClosed));
        assert!(!is_transient(&sqlx::Error::RowNotFound));
        assert!(!is_transient(&sqlx::Error::ColumnNotFound("x".into())));
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_the_row_is_stored() {
        let calls = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let out = store_with_retry(
            "test",
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 3 {
                        Err(io())
                    } else {
                        Ok(())
                    }
                }
            },
            Duration::from_millis(5),
            Duration::from_secs(60),
        )
        .await;
        assert!(out.is_ok());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "three failures, then stored"
        );
        // 5ms + 10ms + 20ms of backoff: the caller commits only after this.
        assert!(started.elapsed() >= Duration::from_millis(35));
    }

    #[tokio::test]
    async fn the_backoff_is_capped() {
        let calls = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        store_with_retry(
            "test",
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 5 {
                        Err(sqlx::Error::PoolTimedOut)
                    } else {
                        Ok(())
                    }
                }
            },
            Duration::from_millis(5),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        // 5 + 10 + 10 + 10 + 10 ms, not 5 + 10 + 20 + 40 + 80.
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(45), "{took:?}");
        assert!(took < Duration::from_millis(500), "{took:?}");
    }

    #[tokio::test]
    async fn a_row_the_schema_rejects_is_returned_not_retried() {
        let calls = AtomicUsize::new(0);
        let out = store_with_retry(
            "test",
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(sqlx::Error::RowNotFound) }
            },
            Duration::from_millis(5),
            Duration::from_secs(60),
        )
        .await;
        assert!(matches!(out, Err(sqlx::Error::RowNotFound)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
