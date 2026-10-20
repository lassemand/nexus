//! Postgres access for `report_releases`.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{PgPool, Row};
use tracing::info;

/// A company eligible for Nasdaq news ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Company {
    pub ticker: String,
    pub exchange_mic: String,
    /// Exact issuer name in the Nasdaq news feed; `None` if unmapped.
    pub nasdaq_news_company: Option<String>,
}

/// A row to insert into `report_releases`.
#[derive(Debug, Clone)]
pub struct NewRelease<'a> {
    pub ticker: &'a str,
    pub exchange_mic: &'a str,
    pub disclosure_id: i64,
    pub released_at: DateTime<Utc>,
    pub category: &'a str,
    pub report_kind: Option<&'static str>,
    pub headline: &'a str,
    pub url: Option<&'a str>,
    pub is_correction: bool,
    pub calendar_update: bool,
}

/// Load all companies on the given MICs, mapped or not.
pub async fn load_companies(pool: &PgPool, mics: &[&str]) -> sqlx::Result<Vec<Company>> {
    let mics: Vec<String> = mics.iter().map(|m| m.to_string()).collect();
    let rows = sqlx::query(
        "SELECT ticker, exchange_mic, nasdaq_news_company FROM companies \
         WHERE exchange_mic = ANY($1) ORDER BY ticker",
    )
    .bind(&mics)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Company {
            ticker: r.get("ticker"),
            exchange_mic: r.get("exchange_mic"),
            nasdaq_news_company: r.get("nasdaq_news_company"),
        })
        .collect())
}

/// What [`record_release`] did with a Nasdaq item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The `disclosure_id` was already stored; the row was refreshed in place.
    Updated,
    /// The item filled in a scheduled row; carries that row's `expected_date`.
    Matched(NaiveDate),
    /// A new row was inserted (expected fields NULL).
    Inserted,
}

/// Days either side of the release's Stockholm date within which a scheduled
/// row is considered to be the same report.
pub const MATCH_WINDOW_DAYS: i64 = 7;

/// Record one Nasdaq item in a single transaction.
///
/// 1. `disclosure_id` already stored → update that row in place (idempotent).
/// 2. A real report (`report_kind` set, not a correction, not a calendar
///    update) → fill the closest unreleased row of the same company whose
///    `expected_date` is within ±[`MATCH_WINDOW_DAYS`] of the release's
///    Europe/Stockholm date (ties: the earlier date), keeping its expected
///    fields.
/// 3. Otherwise insert a new row.
///
/// Corrections, non-reports and calendar updates never match a scheduled row.
pub async fn record_release(pool: &PgPool, r: &NewRelease<'_>) -> sqlx::Result<Recorded> {
    let mut tx = pool.begin().await?;

    let updated = sqlx::query(
        "UPDATE report_releases SET released_at = $2, category = $3, report_kind = $4, \
                headline = $5, url = $6, is_correction = $7, calendar_update = $8, \
                fetched_at = now() \
         WHERE disclosure_id = $1 RETURNING id",
    )
    .bind(r.disclosure_id)
    .bind(r.released_at)
    .bind(r.category)
    .bind(r.report_kind)
    .bind(r.headline)
    .bind(r.url)
    .bind(r.is_correction)
    .bind(r.calendar_update)
    .fetch_optional(&mut *tx)
    .await?;
    if updated.is_some() {
        tx.commit().await?;
        return Ok(Recorded::Updated);
    }

    if r.report_kind.is_some() && !r.is_correction && !r.calendar_update {
        let candidate = sqlx::query(
            "SELECT id, expected_date FROM report_releases \
             WHERE ticker = $1 AND exchange_mic = $2 AND released_at IS NULL \
               AND expected_date BETWEEN \
                   (($3::timestamptz AT TIME ZONE 'Europe/Stockholm')::date - $4::int) AND \
                   (($3::timestamptz AT TIME ZONE 'Europe/Stockholm')::date + $4::int) \
             ORDER BY abs(expected_date - ($3::timestamptz AT TIME ZONE 'Europe/Stockholm')::date), \
                      expected_date \
             LIMIT 1 FOR UPDATE",
        )
        .bind(r.ticker)
        .bind(r.exchange_mic)
        .bind(r.released_at)
        .bind(MATCH_WINDOW_DAYS as i32)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = candidate {
            let id: i64 = row.get("id");
            let expected: NaiveDate = row.get("expected_date");
            sqlx::query(
                "UPDATE report_releases SET disclosure_id = $2, released_at = $3, \
                        category = $4, report_kind = $5, headline = $6, url = $7, \
                        fetched_at = now() \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(r.disclosure_id)
            .bind(r.released_at)
            .bind(r.category)
            .bind(r.report_kind)
            .bind(r.headline)
            .bind(r.url)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            info!(
                ticker = r.ticker,
                expected_date = %expected,
                released_at = %r.released_at,
                "matched scheduled report"
            );
            return Ok(Recorded::Matched(expected));
        }
    }

    let inserted = sqlx::query(
        "INSERT INTO report_releases \
         (ticker, exchange_mic, disclosure_id, released_at, category, report_kind, \
          headline, url, is_correction, calendar_update) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
         ON CONFLICT (disclosure_id) DO NOTHING",
    )
    .bind(r.ticker)
    .bind(r.exchange_mic)
    .bind(r.disclosure_id)
    .bind(r.released_at)
    .bind(r.category)
    .bind(r.report_kind)
    .bind(r.headline)
    .bind(r.url)
    .bind(r.is_correction)
    .bind(r.calendar_update)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // A concurrent run stored the same disclosure between our UPDATE and INSERT.
    Ok(if inserted.rows_affected() == 1 {
        Recorded::Inserted
    } else {
        Recorded::Updated
    })
}

/// Point each report's `superseded_by` at the latest correction published for
/// the same company within 7 days after it. Idempotent; safe to run every time.
pub async fn link_corrections(pool: &PgPool, ticker: &str, mic: &str) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE report_releases o SET superseded_by = c.id \
         FROM ( \
             SELECT DISTINCT ON (o2.id) o2.id AS original_id, c2.id AS id \
             FROM report_releases o2 \
             JOIN report_releases c2 \
               ON c2.ticker = o2.ticker AND c2.exchange_mic = o2.exchange_mic \
              AND c2.is_correction \
              AND c2.released_at > o2.released_at \
              AND c2.released_at <= o2.released_at + interval '7 days' \
             WHERE o2.ticker = $1 AND o2.exchange_mic = $2 \
               AND o2.report_kind IS NOT NULL AND NOT o2.is_correction \
             ORDER BY o2.id, c2.released_at DESC \
         ) c \
         WHERE o.id = c.original_id AND o.superseded_by IS DISTINCT FROM c.id",
    )
    .bind(ticker)
    .bind(mic)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Ephemeral-database helper for tests. Needs `TEST_DATABASE_URL` (a role
/// allowed to CREATE DATABASE); tests skip when it is unset.
#[cfg(test)]
pub mod testutil {
    use sqlx::postgres::PgPoolOptions;
    use sqlx::{Connection, PgConnection, PgPool};
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// A throwaway migrated database containing company `GOMX`/`FNSE`.
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
                "nordic_reports_test_{}_{}",
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
                "INSERT INTO companies (ticker, sector, exchange_mic, currency, nasdaq_news_company) \
                 VALUES ('GOMX', 'Technology', 'FNSE', 'SEK', 'GomSpace Group AB')",
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
}

#[cfg(test)]
mod tests {
    use super::testutil::TestDb;
    use super::*;
    use chrono::TimeZone;

    /// Insert a scheduled (unreleased) row; returns its id.
    async fn schedule(pool: &PgPool, date: &str, label: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO report_releases \
             (ticker, exchange_mic, expected_date, expected_time, calendar_label, fiscal_period) \
             VALUES ('GOMX','FNSE',$1::date,'08:00',$2,'Q2') RETURNING id",
        )
        .bind(date)
        .bind(label)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn release(disclosure_id: i64, released_at: DateTime<Utc>) -> NewRelease<'static> {
        NewRelease {
            ticker: "GOMX",
            exchange_mic: "FNSE",
            disclosure_id,
            released_at,
            category: "Interim report (Q1 and Q3)",
            report_kind: Some("interim"),
            headline: "Q2 report",
            url: Some("https://example.test/x"),
            is_correction: false,
            calendar_update: false,
        }
    }

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    async fn count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM report_releases")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn release_matches_scheduled_row_and_keeps_expected_fields() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        let id = schedule(&db.pool, "2026-08-27", "Q2 report").await;
        let got = record_release(&db.pool, &release(10, at(2026, 8, 26, 6)))
            .await
            .unwrap();
        assert_eq!(got, Recorded::Matched("2026-08-27".parse().unwrap()));
        assert_eq!(count(&db.pool).await, 1);
        let row = sqlx::query(
            "SELECT disclosure_id, released_at, expected_date::text AS d, \
                    expected_time::text AS t, calendar_label, fiscal_period, report_kind, headline \
             FROM report_releases WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(row.get::<Option<i64>, _>("disclosure_id"), Some(10));
        assert_eq!(
            row.get::<Option<DateTime<Utc>>, _>("released_at"),
            Some(at(2026, 8, 26, 6))
        );
        assert_eq!(row.get::<String, _>("d"), "2026-08-27");
        assert_eq!(row.get::<String, _>("t"), "08:00:00");
        assert_eq!(row.get::<String, _>("calendar_label"), "Q2 report");
        assert_eq!(row.get::<String, _>("fiscal_period"), "Q2");
        assert_eq!(row.get::<String, _>("report_kind"), "interim");
        db.drop_db().await;
    }

    #[tokio::test]
    async fn release_without_scheduled_row_inserts() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        let got = record_release(&db.pool, &release(11, at(2026, 8, 26, 6)))
            .await
            .unwrap();
        assert_eq!(got, Recorded::Inserted);
        let expected: Option<NaiveDate> =
            sqlx::query_scalar("SELECT expected_date FROM report_releases")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(expected, None);
        db.drop_db().await;
    }

    #[tokio::test]
    async fn closest_scheduled_date_wins_and_ties_pick_earlier() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        let far = schedule(&db.pool, "2026-08-31", "far").await;
        let near = schedule(&db.pool, "2026-08-27", "near").await;
        let got = record_release(&db.pool, &release(12, at(2026, 8, 26, 6)))
            .await
            .unwrap();
        assert_eq!(got, Recorded::Matched("2026-08-27".parse().unwrap()));
        let matched: Option<i64> =
            sqlx::query_scalar("SELECT id FROM report_releases WHERE disclosure_id = 12")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(matched, Some(near));
        assert_ne!(matched, Some(far));

        // Tie: 2 days either side of 2026-11-10 → the earlier one.
        let early = schedule(&db.pool, "2026-11-08", "early").await;
        schedule(&db.pool, "2026-11-12", "late").await;
        record_release(&db.pool, &release(13, at(2026, 11, 10, 7)))
            .await
            .unwrap();
        let matched: i64 =
            sqlx::query_scalar("SELECT id FROM report_releases WHERE disclosure_id = 13")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(matched, early);
        db.drop_db().await;
    }

    #[tokio::test]
    async fn candidate_outside_seven_days_is_not_matched() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        schedule(&db.pool, "2026-09-05", "too far").await; // 10 days after
        let got = record_release(&db.pool, &release(14, at(2026, 8, 26, 6)))
            .await
            .unwrap();
        assert_eq!(got, Recorded::Inserted);
        assert_eq!(count(&db.pool).await, 2);
        // Exactly 7 days apart still matches.
        schedule(&db.pool, "2026-10-07", "edge").await;
        let got = record_release(&db.pool, &release(15, at(2026, 9, 30, 6)))
            .await
            .unwrap();
        assert!(matches!(got, Recorded::Matched(_)));
        db.drop_db().await;
    }

    #[tokio::test]
    async fn match_uses_stockholm_local_date() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        // 23:30Z on 2026-08-20 is 01:30 on 2026-08-21 in Stockholm; a row
        // expected 2026-08-28 is 7 local days away (matches), 8 UTC days away.
        schedule(&db.pool, "2026-08-28", "edge").await;
        let got = record_release(
            &db.pool,
            &release(16, Utc.with_ymd_and_hms(2026, 8, 20, 23, 30, 0).unwrap()),
        )
        .await
        .unwrap();
        assert!(matches!(got, Recorded::Matched(_)));
        db.drop_db().await;
    }

    #[tokio::test]
    async fn corrections_non_reports_and_calendar_updates_never_match() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        schedule(&db.pool, "2026-08-27", "Q2 report").await;
        let mut correction = release(20, at(2026, 8, 26, 7));
        correction.is_correction = true;
        correction.report_kind = None;
        let mut non_report = release(21, at(2026, 8, 26, 8));
        non_report.report_kind = None;
        let mut calendar = release(22, at(2026, 8, 26, 9));
        calendar.report_kind = None;
        calendar.calendar_update = true;
        // Even with report_kind set, a correction / calendar flag blocks matching.
        let mut flagged = release(23, at(2026, 8, 26, 10));
        flagged.is_correction = true;
        for r in [&correction, &non_report, &calendar, &flagged] {
            assert_eq!(
                record_release(&db.pool, r).await.unwrap(),
                Recorded::Inserted
            );
        }
        let unreleased: i64 =
            sqlx::query_scalar("SELECT count(*) FROM report_releases WHERE released_at IS NULL")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(unreleased, 1);
        db.drop_db().await;
    }

    #[tokio::test]
    async fn rerunning_the_same_feed_creates_no_duplicates() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        schedule(&db.pool, "2026-08-27", "Q2 report").await;
        let r = release(30, at(2026, 8, 26, 6));
        assert!(matches!(
            record_release(&db.pool, &r).await.unwrap(),
            Recorded::Matched(_)
        ));
        assert_eq!(
            record_release(&db.pool, &r).await.unwrap(),
            Recorded::Updated
        );
        let other = release(31, at(2026, 5, 7, 6));
        assert_eq!(
            record_release(&db.pool, &other).await.unwrap(),
            Recorded::Inserted
        );
        assert_eq!(
            record_release(&db.pool, &other).await.unwrap(),
            Recorded::Updated
        );
        assert_eq!(count(&db.pool).await, 2);
        db.drop_db().await;
    }

    #[tokio::test]
    async fn check_constraints_reject_invalid_rows() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        // Neither a release instant nor an expected date.
        let neither = sqlx::query(
            "INSERT INTO report_releases (ticker, exchange_mic) VALUES ('GOMX','FNSE')",
        )
        .execute(&db.pool)
        .await;
        assert!(neither.is_err());
        // disclosure_id without released_at.
        let disclosure_only = sqlx::query(
            "INSERT INTO report_releases (ticker, exchange_mic, disclosure_id, expected_date) \
             VALUES ('GOMX','FNSE',99,'2026-08-27')",
        )
        .execute(&db.pool)
        .await;
        assert!(disclosure_only.is_err());
        // released_at without disclosure_id.
        let released_only = sqlx::query(
            "INSERT INTO report_releases (ticker, exchange_mic, released_at) \
             VALUES ('GOMX','FNSE', now())",
        )
        .execute(&db.pool)
        .await;
        assert!(released_only.is_err());
        // Duplicate scheduled row is rejected by the partial unique index.
        schedule(&db.pool, "2026-08-27", "dup").await;
        let dup = sqlx::query(
            "INSERT INTO report_releases (ticker, exchange_mic, expected_date, calendar_label) \
             VALUES ('GOMX','FNSE','2026-08-27','dup')",
        )
        .execute(&db.pool)
        .await;
        assert!(dup.is_err());
        db.drop_db().await;
    }
}
