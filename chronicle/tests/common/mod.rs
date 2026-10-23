//! Ephemeral-database helper for the Nordic integration tests. Needs
//! `TEST_DATABASE_URL` (a role allowed to CREATE DATABASE); tests skip when unset.
#![allow(dead_code)]

use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A throwaway migrated database containing company `GOMX`/`FNSE`
/// (MFN slug `gomspace-group`).
pub struct TestDb {
    pub pool: PgPool,
    admin: PgConnection,
    name: String,
}

impl TestDb {
    /// Create the fully migrated database, or `None` if `TEST_DATABASE_URL` is unset.
    pub async fn create() -> Option<Self> {
        let db = Self::create_empty().await?;
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db.add_company("GOMX", "FNSE", "gomspace-group").await;
        Some(db)
    }

    /// Create an empty database (no migrations applied).
    pub async fn create_empty() -> Option<Self> {
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
        Some(Self { pool, admin, name })
    }

    /// Insert a company with an MFN slug (needs the companies migrations).
    pub async fn add_company(&self, ticker: &str, mic: &str, slug: &str) {
        sqlx::query(
            "INSERT INTO companies (ticker, sector, exchange_mic, currency, mfn_slug) \
             VALUES ($1, 'Technology', $2, 'SEK', $3)",
        )
        .bind(ticker)
        .bind(mic)
        .bind(slug)
        .execute(&self.pool)
        .await
        .unwrap();
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
