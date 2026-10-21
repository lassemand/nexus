//! Ephemeral-database helper for the Nordic integration tests. Needs
//! `TEST_DATABASE_URL` (a role allowed to CREATE DATABASE); tests skip when unset.
#![allow(dead_code)]

use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A throwaway migrated database containing company `GOMX`/`FNSE`
/// (Nasdaq issuer `GomSpace Group AB`, MFN slug `gomspace-group`).
pub struct TestDb {
    pub pool: PgPool,
    admin: PgConnection,
    name: String,
}

impl TestDb {
    /// Create the database, or `None` if `TEST_DATABASE_URL` is unset.
    pub async fn create() -> Option<Self> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let name = format!(
            "nordic_test_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        let mut admin = PgConnection::connect(&url).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let (base, _) = url.rsplit_once('/').expect("database url has a path");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&format!("{base}/{name}"))
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO companies (ticker, sector, exchange_mic, currency, nasdaq_news_company, mfn_slug) \
             VALUES ('GOMX', 'Technology', 'FNSE', 'SEK', 'GomSpace Group AB', 'gomspace-group')",
        )
        .execute(&pool)
        .await
        .unwrap();
        Some(Self { pool, admin, name })
    }

    /// Drop the database.
    pub async fn drop_db(mut self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {}", self.name))
            .execute(&mut self.admin)
            .await
            .unwrap();
    }
}
