//! The scheduler against two real databases: the analyst's, and signal's real
//! schema in a second Postgres schema of the same test database.

mod common;

use analyst::{RunResult, TargetStatus};
use common::{date, hours, time, utc, Settings, World};
use sqlx::PgPool;

#[sqlx::test]
async fn a_report_at_0800_stockholm_is_due_at_2000_the_day_before(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 18, 0)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;

    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!((report.created, report.started), (1, 0));
    let target = w.target("AAA").await;
    assert_eq!(target.expected_at, utc(2026, 12, 10, 7, 0));
    assert!(target.expected_time_known);
    assert_eq!(target.prep_due_at, utc(2026, 12, 9, 19, 0));
    assert_eq!(target.status().expect("status"), TargetStatus::Waiting);

    w.clock.set(utc(2026, 12, 9, 18, 59));
    assert_eq!(w.scheduler.pass().await.expect("pass").started, 0);

    w.clock.set(utc(2026, 12, 9, 19, 0));
    assert_eq!(w.scheduler.pass().await.expect("pass").started, 1);
    let launched = w.launcher.launched();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].attempt, 1);
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Preparing
    );
}

#[sqlx::test]
async fn a_date_only_report_on_a_helsinki_company_is_due_at_1200_helsinki_the_day_before(
    pool: PgPool,
) {
    let w = World::new(pool, utc(2026, 12, 9, 6, 0)).await;
    w.add_company("FIN", "XHEL").await;
    w.schedule("FIN", "XHEL", date(2026, 12, 10), None).await;

    w.scheduler.pass().await.expect("pass");
    let target = w.target("FIN").await;
    assert!(!target.expected_time_known);
    assert_eq!(target.expected_at, utc(2026, 12, 9, 22, 0));
    assert_eq!(target.prep_due_at, utc(2026, 12, 9, 10, 0));

    w.clock.set(utc(2026, 12, 9, 9, 59));
    assert_eq!(w.scheduler.pass().await.expect("pass").started, 0);
    w.clock.set(utc(2026, 12, 9, 10, 0));
    assert_eq!(w.scheduler.pass().await.expect("pass").started, 1);
}

#[sqlx::test]
async fn a_date_moved_earlier_keeps_the_kernel(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 7, 12, 0)).await;
    w.add_company("AAA", "XSTO").await;
    let row = w
        .schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    let id = w.target("AAA").await.id;
    w.make_ready(id).await;

    w.move_report(row, date(2026, 12, 8), Some(time(9, 0)))
        .await;
    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!(
        (report.updated, report.reprepared, report.started),
        (1, 0, 0)
    );
    let target = w.target("AAA").await;
    assert_eq!(target.status().expect("status"), TargetStatus::Ready);
    assert_eq!(target.expected_at, utc(2026, 12, 8, 8, 0));
    assert!(w.store.accepted_kernel(id).await.expect("query").is_some());
    assert!(w.launcher.launched().is_empty());
}

#[sqlx::test]
async fn a_date_moved_three_weeks_later_prepares_again_at_the_new_time(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 7, 12, 0)).await;
    w.add_company("AAA", "XSTO").await;
    let row = w
        .schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    let id = w.target("AAA").await.id;
    w.make_ready(id).await;

    w.move_report(row, date(2026, 12, 31), Some(time(8, 0)))
        .await;
    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!((report.updated, report.reprepared), (1, 1));
    let target = w.target("AAA").await;
    assert_eq!(target.status().expect("status"), TargetStatus::Waiting);
    assert_eq!(target.prep_due_at, utc(2026, 12, 30, 19, 0));
    assert_eq!(target.prep_attempts, 0);
    assert!(
        w.store.accepted_kernel(id).await.expect("query").is_some(),
        "the old kernel stays until a new one is accepted"
    );

    w.clock.set(utc(2026, 12, 30, 18, 59));
    assert_eq!(w.scheduler.pass().await.expect("pass").started, 0);
    w.clock.set(utc(2026, 12, 30, 19, 0));
    assert_eq!(w.scheduler.pass().await.expect("pass").started, 1);
    assert_eq!(w.launcher.launched().len(), 1);
}

#[sqlx::test]
async fn a_date_moved_later_by_less_than_the_lead_keeps_the_kernel(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 20, 0)).await;
    w.add_company("AAA", "XSTO").await;
    let row = w
        .schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    let id = w.target("AAA").await.id;
    w.make_ready(id).await;

    w.move_report(row, date(2026, 12, 10), Some(time(15, 0)))
        .await;
    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!((report.updated, report.reprepared), (1, 0));
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Ready
    );
}

#[sqlx::test]
async fn a_report_removed_from_the_schedule_is_cancelled(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 8, 12, 0)).await;
    w.add_company("AAA", "XSTO").await;
    let row = w
        .schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Waiting
    );

    w.remove_report(row).await;
    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!(report.cancelled, 1);
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Cancelled
    );
}

#[sqlx::test]
async fn cancelling_a_preparing_target_closes_its_running_run(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 19, 30)).await;
    w.add_company("AAA", "XSTO").await;
    let row = w
        .schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    assert_eq!(w.scheduler.pass().await.expect("pass").started, 1);

    w.remove_report(row).await;
    assert_eq!(w.scheduler.pass().await.expect("pass").cancelled, 1);
    let outcome: (String, Option<String>) =
        sqlx::query_as("SELECT outcome, detail FROM prep_runs ORDER BY id LIMIT 1")
            .fetch_one(&w.analyst)
            .await
            .expect("run");
    assert_eq!(
        outcome,
        ("failed".to_owned(), Some("target cancelled".to_owned()))
    );
}

#[sqlx::test]
async fn a_ready_target_survives_its_report_leaving_the_schedule(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 20, 0)).await;
    w.add_company("AAA", "XSTO").await;
    let row = w
        .schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    let id = w.target("AAA").await.id;
    w.make_ready(id).await;

    w.remove_report(row).await;
    assert_eq!(w.scheduler.pass().await.expect("pass").cancelled, 0);
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Ready
    );
}

#[sqlx::test]
async fn failed_launches_stop_at_the_attempt_limit(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 19, 0)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.launcher.fail_with(Some("boom"));

    let first = w.scheduler.pass().await.expect("pass");
    assert_eq!((first.started, first.launch_failed), (0, 1));
    let target = w.target("AAA").await;
    assert_eq!(target.status().expect("status"), TargetStatus::Waiting);
    assert_eq!(target.retry_after, Some(utc(2026, 12, 9, 19, 5)));

    assert_eq!(
        w.scheduler.pass().await.expect("pass").launch_failed,
        0,
        "not before the backoff"
    );
    w.clock.set(utc(2026, 12, 9, 19, 5));
    assert_eq!(w.scheduler.pass().await.expect("pass").launch_failed, 1);
    w.clock.set(utc(2026, 12, 9, 19, 10));
    assert_eq!(w.scheduler.pass().await.expect("pass").launch_failed, 1);
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Failed
    );
    assert_eq!(w.launcher.launched().len(), 3);

    w.clock.set(utc(2026, 12, 9, 21, 0));
    assert_eq!(w.scheduler.pass().await.expect("pass").launch_failed, 0);
    assert_eq!(
        w.launcher.launched().len(),
        3,
        "a failed target is not tried again"
    );
}

#[sqlx::test]
async fn a_failed_run_reported_later_retries_after_the_backoff(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 19, 0)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    let run = w.launcher.launched()[0].run_id;

    w.clock.set(utc(2026, 12, 9, 19, 20));
    w.scheduler
        .finish_run(run, RunResult::TimedOut, Some("deadline"))
        .await
        .expect("finish");
    let target = w.target("AAA").await;
    assert_eq!(target.status().expect("status"), TargetStatus::Waiting);
    assert_eq!(target.retry_after, Some(utc(2026, 12, 9, 19, 25)));
    assert!(
        w.scheduler
            .finish_run(run, RunResult::Failed, None)
            .await
            .is_err(),
        "a run finishes once"
    );

    w.clock.set(utc(2026, 12, 9, 19, 25));
    assert_eq!(w.scheduler.pass().await.expect("pass").started, 1);
    assert_eq!(w.launcher.launched()[1].attempt, 2);
}

#[sqlx::test]
async fn a_successful_run_makes_the_target_ready(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 19, 0)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    let run = w.launcher.launched()[0].run_id;

    w.scheduler
        .finish_run(run, RunResult::Succeeded, None)
        .await
        .expect("finish");
    let target = w.target("AAA").await;
    assert_eq!(target.status().expect("status"), TargetStatus::Ready);
    assert_eq!(target.prepared_for_at, Some(utc(2026, 12, 10, 7, 0)));
    assert_eq!(target.prep_attempts, 0);
}

#[sqlx::test]
async fn no_preparation_starts_inside_the_minimum_lead(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 10, 6, 30)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    for minute in [30, 45, 59] {
        w.clock.set(utc(2026, 12, 10, 6, minute));
        assert_eq!(w.scheduler.pass().await.expect("pass").started, 0);
    }
    assert!(w.launcher.launched().is_empty());
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Waiting
    );
}

#[sqlx::test]
async fn a_retry_that_would_land_inside_the_minimum_lead_gives_up(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 10, 5, 58)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.launcher.fail_with(Some("boom"));
    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!(report.launch_failed, 1);
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Failed
    );
}

#[sqlx::test]
async fn the_allowlist_restricts_the_targets(pool: PgPool) {
    let settings = Settings {
        allowlist: vec![("AAA".to_owned(), "XSTO".to_owned())],
        ..Settings::default()
    };
    let w = World::with(pool, utc(2026, 12, 9, 12, 0), settings).await;
    for ticker in ["AAA", "BBB"] {
        w.add_company(ticker, "XSTO").await;
        w.schedule(ticker, "XSTO", date(2026, 12, 10), Some(time(8, 0)))
            .await;
    }
    w.scheduler.pass().await.expect("pass");
    assert_eq!(w.target_count().await, 1);
    assert_eq!(w.target("AAA").await.ticker, "AAA");
}

#[sqlx::test]
async fn concurrency_never_exceeds_the_limit_and_soonest_goes_first(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 23, 30)).await;
    let times = [(10, 0), (8, 0), (9, 0), (11, 0), (12, 0)];
    for (i, (h, m)) in times.iter().enumerate() {
        let ticker = format!("C{i}");
        w.add_company(&ticker, "XSTO").await;
        w.schedule(&ticker, "XSTO", date(2026, 12, 10), Some(time(*h, *m)))
            .await;
    }
    assert_eq!(w.scheduler.pass().await.expect("pass").started, 2);
    let first: Vec<String> = w
        .launcher
        .launched()
        .iter()
        .map(|r| r.target.ticker.clone())
        .collect();
    assert_eq!(
        first,
        vec!["C1".to_owned(), "C2".to_owned()],
        "08:00 and 09:00 go first"
    );
    assert_eq!(w.store.preparing_count().await.expect("count"), 2);

    assert_eq!(w.scheduler.pass().await.expect("pass").started, 0);
    assert_eq!(w.store.preparing_count().await.expect("count"), 2);

    w.scheduler
        .finish_run(w.launcher.launched()[0].run_id, RunResult::Succeeded, None)
        .await
        .expect("finish");
    assert_eq!(w.scheduler.pass().await.expect("pass").started, 1);
    assert_eq!(w.store.preparing_count().await.expect("count"), 2);
    assert_eq!(w.launcher.launched()[2].target.ticker, "C0");
}

#[sqlx::test]
async fn a_report_far_ahead_gets_no_target_until_it_is_within_the_look_ahead(pool: PgPool) {
    let w = World::new(pool, utc(2026, 11, 1, 12, 0)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    assert_eq!(w.target_count().await, 0);

    w.clock.set(utc(2026, 12, 2, 12, 0));
    assert_eq!(
        w.scheduler.pass().await.expect("pass").created,
        0,
        "7 days 19 hours ahead is outside the 7 days 12 hours"
    );

    w.clock.set(utc(2026, 12, 3, 0, 0));
    assert_eq!(w.scheduler.pass().await.expect("pass").created, 1);
}

#[sqlx::test]
async fn the_other_calendar_source_winning_keeps_the_same_target(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 8, 12, 0)).await;
    w.add_company("AAA", "XSTO").await;
    let page = w
        .schedule_with(
            "AAA",
            "XSTO",
            date(2026, 12, 10),
            Some(time(8, 0)),
            None,
            "mfn",
        )
        .await;
    w.scheduler.pass().await.expect("pass");
    let before = w.target("AAA").await;

    let release = w
        .schedule_with(
            "AAA",
            "XSTO",
            date(2026, 12, 11),
            Some(time(8, 0)),
            None,
            "mfn_release",
        )
        .await;
    sqlx::query("UPDATE special_events SET superseded_by = $2 WHERE id = $1")
        .bind(page)
        .bind(release)
        .execute(&w.signal_writer)
        .await
        .expect("superseded");
    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!(
        (report.created, report.updated, report.cancelled),
        (0, 1, 0)
    );
    let after = w.target("AAA").await;
    assert_eq!(after.id, before.id);
    assert_eq!(after.signal_event_id, release);
    assert_eq!(after.expected_date, date(2026, 12, 11));
    assert_eq!(w.target_count().await, 1);
}

#[sqlx::test]
async fn losing_and_other_sources_are_not_scheduled(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 8, 12, 0)).await;
    w.add_company("AAA", "XSTO").await;
    let loser = w
        .schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    let winner = w
        .schedule_with(
            "AAA",
            "XSTO",
            date(2026, 12, 11),
            Some(time(8, 0)),
            None,
            "mfn_release",
        )
        .await;
    sqlx::query("UPDATE special_events SET superseded_by = $2 WHERE id = $1")
        .bind(loser)
        .bind(winner)
        .execute(&w.signal_writer)
        .await
        .expect("superseded");
    w.schedule_with(
        "AAA",
        "XSTO",
        date(2026, 12, 12),
        Some(time(8, 0)),
        None,
        "other",
    )
    .await;
    w.scheduler.pass().await.expect("pass");
    assert_eq!(w.target_count().await, 1);
    assert_eq!(w.target("AAA").await.signal_event_id, winner);
}

#[sqlx::test]
async fn a_repeated_pass_changes_nothing(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 12, 0)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    let again = w.scheduler.pass().await.expect("pass");
    assert_eq!(
        (again.created, again.updated, again.cancelled, again.started),
        (0, 0, 0, 0)
    );
    assert_eq!(w.target_count().await, 1);
    let _ = hours(1);
}
