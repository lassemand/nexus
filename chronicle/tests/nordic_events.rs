//! Publishing Nordic reports as special events (NEX-171). Skipped unless
//! `TEST_DATABASE_URL` is set.

mod common;

use async_trait::async_trait;
use chronicle_lib::nordic::db::{record_release, NewRelease};
use chronicle_lib::nordic::events::{sync, EventSink, Synced};
use chrono::{TimeZone, Utc};
use common::TestDb;
use model::generated::SpecialEvent;
use sqlx::PgPool;
use std::sync::Mutex;
use uuid::Uuid;

/// Collects events; fails every publish once `fail_after` events went out.
#[derive(Default)]
struct VecSink {
    sent: Mutex<Vec<SpecialEvent>>,
    fail_after: Option<usize>,
}

#[async_trait]
impl EventSink for VecSink {
    async fn publish(&self, event: &SpecialEvent) -> anyhow::Result<()> {
        let mut sent = self.sent.lock().unwrap();
        if self.fail_after.is_some_and(|n| sent.len() >= n) {
            anyhow::bail!("broker down");
        }
        sent.push(event.clone());
        Ok(())
    }
}

impl VecSink {
    fn take(&self) -> Vec<SpecialEvent> {
        std::mem::take(&mut self.sent.lock().unwrap())
    }
}

async fn schedule(pool: &PgPool, date: &str, kind: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO report_releases \
           (ticker, exchange_mic, expected_date, expected_time, calendar_label, fiscal_period, report_kind) \
         VALUES ('GOMX','FNSE',$1::date,'07:30','Kvartalsrapport','2026-Q3',$2) RETURNING id",
    )
    .bind(date)
    .bind(kind)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn release(n: u128, day: u32) -> NewRelease<'static> {
    NewRelease {
        ticker: "GOMX",
        exchange_mic: "FNSE",
        mfn_news_id: Uuid::from_u128(n),
        occurred_at: Utc.with_ymd_and_hms(2026, 11, day, 6, 30, 0).unwrap(),
        report_kind: Some("interim"),
        headline: "GomSpace interim report Q3",
        url: None,
        lang: Some("en"),
        tags: &[],
        fiscal_period: None,
        is_correction: false,
        calendar_update: false,
    }
}

#[tokio::test]
async fn reports_are_published_once_and_follow_their_row() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = &db.pool;
    let sink = VecSink::default();

    // A scheduled report, and a news item that is not a report.
    let id = schedule(pool, "2026-11-12", "interim").await;
    sqlx::query(
        "INSERT INTO report_releases (ticker, exchange_mic, mfn_news_id, occurred_at, headline) \
         VALUES ('GOMX','FNSE',$1,now(),'Order intake')",
    )
    .bind(Uuid::from_u128(99))
    .execute(pool)
    .await
    .unwrap();

    assert_eq!(
        sync(pool, &sink).await.unwrap(),
        Synced {
            upserted: 1,
            removed: 0
        }
    );
    let sent = sink.take();
    let key = format!("report:{id}");
    assert_eq!(sent[0].source_key, key);
    assert!(sent[0].scheduled);
    assert_eq!(sent[0].event_type, "quarterly report");
    // 07:30 Stockholm (CET in November) is 06:30 UTC.
    assert_eq!(
        sent[0].occurred_at_unix_secs,
        Utc.with_ymd_and_hms(2026, 11, 12, 6, 30, 0)
            .unwrap()
            .timestamp()
    );
    assert!(
        sent[0].description.contains("(estimated)"),
        "{}",
        sent[0].description
    );

    // Nothing changed: nothing is published.
    assert_eq!(sync(pool, &sink).await.unwrap(), Synced::default());

    // MFN moves the date: same key, new time.
    sqlx::query("UPDATE report_releases SET expected_date = '2026-11-14' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(sync(pool, &sink).await.unwrap().upserted, 1);
    assert_eq!(sink.take()[0].source_key, key);

    // The report comes out and fills the scheduled row: it flips to occurred.
    record_release(pool, &release(1, 14)).await.unwrap();
    assert_eq!(sync(pool, &sink).await.unwrap().upserted, 1);
    let flipped = sink.take();
    assert_eq!(flipped[0].source_key, key);
    assert!(!flipped[0].scheduled);
    assert!(!flipped[0].description.contains("estimated"));

    // The row goes away: a removal is published once.
    sqlx::query("DELETE FROM report_releases WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        sync(pool, &sink).await.unwrap(),
        Synced {
            upserted: 0,
            removed: 1
        }
    );
    let removal = sink.take();
    assert!(removal[0].removed);
    assert_eq!(removal[0].source_key, key);
    assert_eq!(sync(pool, &sink).await.unwrap(), Synced::default());

    db.drop_db().await;
}

#[tokio::test]
async fn undelivered_events_stay_pending() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = &db.pool;
    schedule(pool, "2026-11-12", "interim").await;
    schedule(pool, "2027-02-18", "annual").await;

    let flaky = VecSink {
        fail_after: Some(1),
        ..Default::default()
    };
    assert!(sync(pool, &flaky).await.is_err());
    assert_eq!(flaky.take().len(), 1);

    let ok = VecSink::default();
    assert_eq!(sync(pool, &ok).await.unwrap().upserted, 1);
    assert_eq!(ok.take()[0].event_type, "annual report");

    db.drop_db().await;
}
