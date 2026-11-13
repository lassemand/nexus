//! Recording decisions and matching releases to targets, against a real Postgres.

use analyst::{DecisionKind, NewDecision, Recorded, Release, ReportKind, Store};
use analyst::{MatchKind, TargetStatus};
use chrono::{DateTime, Duration, TimeZone, Utc};
use sqlx::PgPool;

fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 5, h, m, s)
        .single()
        .expect("valid instant")
}

async fn target(
    pool: &PgPool,
    ticker: &str,
    period: Option<&str>,
    expected: &str,
    status: &str,
) -> i64 {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO report_targets (ticker, exchange_mic, report_kind, fiscal_period, expected_date, \
             expected_time_known, expected_at, signal_event_id, status, prep_due_at, created_at, updated_at) \
         VALUES ($1, 'XSTO', 'interim', $2, $3::date, true, now(), floor(random() * 1e9)::bigint, $4, now(), now(), now()) \
         RETURNING id",
    )
    .bind(ticker)
    .bind(period)
    .bind(expected)
    .bind(status)
    .fetch_one(pool)
    .await
    .expect("target");
    row.0
}

async fn kernel(pool: &PgPool, target_id: i64, status: &str) -> i64 {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO kernels (target_id, version, source, wasm, sha256, status, created_at) \
         VALUES ($1, 1, 'src', '\\x00'::bytea, 'abc', $2, now()) RETURNING id",
    )
    .bind(target_id)
    .bind(status)
    .fetch_one(pool)
    .await
    .expect("kernel");
    row.0
}

async fn status_of(pool: &PgPool, table: &str, id: i64) -> String {
    let row: (String,) = sqlx::query_as(&format!("SELECT status FROM {table} WHERE id = $1"))
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("status");
    row.0
}

fn decision(
    news_id: &str,
    target_id: Option<i64>,
    kernel_id: Option<i64>,
    kind: DecisionKind,
) -> NewDecision {
    NewDecision {
        target_id,
        kernel_id,
        news_id: news_id.to_owned(),
        ticker: "AAA".into(),
        exchange_mic: "XSTO".into(),
        lang: "en".into(),
        fiscal_period: Some("2026-Q3".into()),
        published_at: at(7, 0, 0),
        received_at: at(7, 0, 2),
        decided_at: at(7, 0, 2),
        decision: kind,
        reason: Some("because".into()),
        metrics: Vec::new(),
        kernel_micros: Some(17),
        error: None,
        trace_id: Some("0af7651916cd43dd8448eb211c80319c".into()),
    }
}

#[sqlx::test]
async fn a_kernel_decision_settles_the_target_and_spends_the_kernel(pool: PgPool) {
    let store = Store::new(pool.clone());
    let t = target(&pool, "AAA", Some("2026-Q3"), "2026-11-05", "ready").await;
    let k = kernel(&pool, t, "accepted").await;
    sqlx::query("INSERT INTO prep_runs (target_id, started_at, attempt, prepared_for_at) VALUES ($1, now(), 1, now())")
        .bind(t)
        .execute(&pool)
        .await
        .expect("run");

    let recorded = store
        .record_decision(&decision("n1", Some(t), Some(k), DecisionKind::Buy))
        .await
        .expect("recorded");
    assert!(matches!(
        recorded,
        Recorded::Inserted {
            kind: DecisionKind::Buy,
            ..
        }
    ));
    assert_eq!(status_of(&pool, "report_targets", t).await, "decided");
    assert_eq!(status_of(&pool, "kernels", k).await, "spent");
    let run: (String, Option<String>) = sqlx::query_as("SELECT outcome, detail FROM prep_runs")
        .fetch_one(&pool)
        .await
        .expect("run");
    assert_eq!(
        run,
        ("failed".to_owned(), Some("report released".to_owned()))
    );

    let stored = store
        .decision_by_news_id("n1")
        .await
        .expect("query")
        .expect("stored");
    assert_eq!(stored.kind(), Ok(DecisionKind::Buy));
    assert_eq!(stored.target_id, Some(t));
    assert_eq!(stored.kernel_micros, Some(17));
    assert_eq!(
        stored.trace_id.as_deref(),
        Some("0af7651916cd43dd8448eb211c80319c")
    );
}

#[sqlx::test]
async fn a_redelivery_changes_nothing(pool: PgPool) {
    let store = Store::new(pool.clone());
    let t = target(&pool, "AAA", Some("2026-Q3"), "2026-11-05", "ready").await;
    store
        .record_decision(&decision("n1", Some(t), None, DecisionKind::Abstain))
        .await
        .expect("first");
    let again = store
        .record_decision(&decision("n1", Some(t), None, DecisionKind::Buy))
        .await
        .expect("second");
    assert_eq!(again, Recorded::Duplicate);
    let rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM decisions")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows.0, 1);
    let stored = store
        .decision_by_news_id("n1")
        .await
        .expect("query")
        .expect("stored");
    assert_eq!(
        stored.kind(),
        Ok(DecisionKind::Abstain),
        "the first decision stands"
    );
}

#[sqlx::test]
async fn a_second_version_of_a_settled_report_is_stored_as_a_twin(pool: PgPool) {
    let store = Store::new(pool.clone());
    let t = target(&pool, "AAA", Some("2026-Q3"), "2026-11-05", "ready").await;
    let k = kernel(&pool, t, "accepted").await;
    store
        .record_decision(&decision("sv", Some(t), Some(k), DecisionKind::NoBuy))
        .await
        .expect("first");

    let twin = store
        .record_decision(&decision("en", Some(t), Some(k), DecisionKind::Buy))
        .await
        .expect("second");
    assert!(matches!(
        twin,
        Recorded::Inserted {
            kind: DecisionKind::TwinIgnored,
            ..
        }
    ));
    let stored = store
        .decision_by_news_id("en")
        .await
        .expect("query")
        .expect("stored");
    assert_eq!(stored.kind(), Ok(DecisionKind::TwinIgnored));
    assert_eq!(
        stored.reason.as_deref(),
        Some("the report is already decided")
    );
    assert_eq!(status_of(&pool, "report_targets", t).await, "decided");
}

#[sqlx::test]
async fn two_versions_racing_give_one_decision(pool: PgPool) {
    let store = Store::new(pool.clone());
    let t = target(&pool, "AAA", Some("2026-Q3"), "2026-11-05", "ready").await;
    let sv = decision("sv", Some(t), None, DecisionKind::Buy);
    let en = decision("en", Some(t), None, DecisionKind::Buy);
    let (a, b) = tokio::join!(store.record_decision(&sv), store.record_decision(&en));
    let kinds: Vec<DecisionKind> = [a, b]
        .into_iter()
        .map(|r| match r.expect("recorded") {
            Recorded::Inserted { kind, .. } => kind,
            Recorded::Duplicate => panic!("not a duplicate"),
        })
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == DecisionKind::Buy).count(),
        1,
        "{kinds:?}"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == DecisionKind::TwinIgnored)
            .count(),
        1,
        "{kinds:?}"
    );
}

#[sqlx::test]
async fn stale_and_no_kernel_skip_the_target_and_a_twin_changes_nothing(pool: PgPool) {
    let store = Store::new(pool.clone());
    let stale_target = target(&pool, "AAA", Some("2026-Q3"), "2026-11-05", "ready").await;
    let stale_kernel = kernel(&pool, stale_target, "accepted").await;
    store
        .record_decision(&decision(
            "old",
            Some(stale_target),
            Some(stale_kernel),
            DecisionKind::Stale,
        ))
        .await
        .expect("stale");
    assert_eq!(
        status_of(&pool, "report_targets", stale_target).await,
        "skipped"
    );
    assert_eq!(status_of(&pool, "kernels", stale_kernel).await, "spent");

    let waiting = target(&pool, "BBB", Some("2026-Q3"), "2026-11-05", "waiting").await;
    store
        .record_decision(&decision(
            "nokernel",
            Some(waiting),
            None,
            DecisionKind::SkippedNoKernel,
        ))
        .await
        .expect("no kernel");
    assert_eq!(status_of(&pool, "report_targets", waiting).await, "skipped");

    let ready = target(&pool, "CCC", Some("2026-Q3"), "2026-11-05", "ready").await;
    store
        .record_decision(&decision(
            "twin",
            Some(ready),
            None,
            DecisionKind::TwinIgnored,
        ))
        .await
        .expect("twin");
    assert_eq!(
        status_of(&pool, "report_targets", ready).await,
        "ready",
        "a twin changes nothing"
    );
}

#[sqlx::test]
async fn an_unmatched_release_is_recorded_without_a_target(pool: PgPool) {
    let store = Store::new(pool.clone());
    let recorded = store
        .record_decision(&decision(
            "early",
            None,
            None,
            DecisionKind::SkippedNoKernel,
        ))
        .await
        .expect("recorded");
    assert!(matches!(
        recorded,
        Recorded::Inserted {
            kind: DecisionKind::SkippedNoKernel,
            ..
        }
    ));
    let stored = store
        .decision_by_news_id("early")
        .await
        .expect("query")
        .expect("stored");
    assert_eq!(stored.target_id, None);
}

#[sqlx::test]
async fn the_database_rejects_bad_decisions_and_trace_ids(pool: PgPool) {
    let store = Store::new(pool.clone());
    let mut bad_trace = decision("t1", None, None, DecisionKind::Abstain);
    bad_trace.trace_id = Some("not-a-trace-id".into());
    assert!(
        store.record_decision(&bad_trace).await.is_err(),
        "a trace id is 32 lowercase hex"
    );
    let mut upper = decision("t2", None, None, DecisionKind::Abstain);
    upper.trace_id = Some("0AF7651916CD43DD8448EB211C80319C".into());
    assert!(
        store.record_decision(&upper).await.is_err(),
        "lowercase only"
    );
    let mut none = decision("t3", None, None, DecisionKind::Abstain);
    none.trace_id = None;
    assert!(
        store.record_decision(&none).await.is_ok(),
        "no trace id is fine"
    );
    let bad_kind = sqlx::query(
        "INSERT INTO decisions (news_id, ticker, exchange_mic, lang, published_at, received_at, decided_at, decision) \
         VALUES ('x', 'A', 'XSTO', 'en', now(), now(), now(), 'maybe')",
    )
    .execute(&pool)
    .await;
    assert!(bad_kind.is_err(), "unknown decision value");
}

#[sqlx::test]
async fn twins_are_found_by_target_or_by_time(pool: PgPool) {
    let store = Store::new(pool.clone());
    let t = target(&pool, "AAA", Some("2026-Q3"), "2026-11-05", "ready").await;
    store
        .record_decision(&decision("sv", Some(t), None, DecisionKind::Buy))
        .await
        .expect("first");

    let by_target = store
        .earlier_decision(
            "AAA",
            "XSTO",
            Some(t),
            at(7, 5, 0) + Duration::hours(3),
            "en",
        )
        .await
        .expect("query")
        .expect("found by target");
    assert_eq!(by_target.news_id, "sv");

    store
        .record_decision(&decision(
            "unscheduled-sv",
            None,
            None,
            DecisionKind::SkippedNoKernel,
        ))
        .await
        .expect("no target");
    let by_time = store
        .earlier_decision("AAA", "XSTO", None, at(7, 0, 25), "unscheduled-en")
        .await
        .expect("query");
    assert!(
        by_time.is_some(),
        "the English version 25 s later is a twin"
    );
    let other_company = store
        .earlier_decision("BBB", "XSTO", None, at(7, 0, 25), "x")
        .await
        .expect("query");
    assert!(other_company.is_none());
    let much_later = store
        .earlier_decision("AAA", "XSTO", None, at(9, 0, 0), "x")
        .await
        .expect("query");
    assert!(much_later.is_none(), "two hours later is another release");
    let itself = store
        .earlier_decision("AAA", "XSTO", None, at(7, 0, 0), "sv")
        .await
        .expect("query");
    assert!(
        itself.is_none() || itself.is_some_and(|d| d.news_id != "sv"),
        "a release is not its own twin"
    );
}

#[sqlx::test]
async fn match_target_reads_the_companys_targets(pool: PgPool) {
    let store = Store::new(pool.clone());
    let q2 = target(&pool, "AAA", Some("2026-Q2"), "2026-08-20", "decided").await;
    let q3 = target(&pool, "AAA", Some("2026-Q3"), "2026-11-05", "ready").await;
    target(&pool, "BBB", Some("2026-Q3"), "2026-11-05", "ready").await;
    target(&pool, "AAA", Some("2026-Q4"), "2026-11-05", "cancelled").await;
    let release = |period: Option<&str>| Release {
        ticker: "AAA".into(),
        exchange_mic: "XSTO".into(),
        kind: ReportKind::Interim,
        fiscal_period: period.map(str::to_owned),
        published_at: at(7, 0, 0),
    };

    let by_period = store
        .match_target(&release(Some("2026-Q3")))
        .await
        .expect("matched");
    assert_eq!(
        (by_period.target.map(|t| t.id), by_period.how),
        (Some(q3), MatchKind::FiscalPeriod)
    );
    let by_date = store.match_target(&release(None)).await.expect("matched");
    assert_eq!(
        (by_date.target.map(|t| t.id), by_date.how),
        (Some(q3), MatchKind::NearestDate)
    );
    let old = store
        .match_target(&release(Some("2026-Q2")))
        .await
        .expect("matched");
    assert_eq!(
        old.target.map(|t| (t.id, t.status)),
        Some((q2, "decided".to_owned()))
    );
    let unknown = store
        .match_target(&release(Some("2027-Q1")))
        .await
        .expect("matched");
    assert_eq!(
        (unknown.target, unknown.how),
        (None, MatchKind::None),
        "every target has another period, and different periods are different reports"
    );
    let nowhere = {
        let mut r = release(None);
        r.published_at = at(7, 0, 0) - Duration::days(30);
        store.match_target(&r).await.expect("matched")
    };
    assert_eq!((nowhere.target, nowhere.how), (None, MatchKind::None));
    let _ = TargetStatus::Ready;
}
