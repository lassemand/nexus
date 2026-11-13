//! The notes consumer's handler and how notes drive the scheduler.

mod common;

use std::time::Duration;

use analyst::{NoteOutcome, TargetStatus};
use chrono::Duration as ChronoDuration;
use common::{date, note_payload, time, utc, World};
use sqlx::PgPool;

const NOTE_A: &str = "6f9619ff-8b86-d011-b42d-00cf4fc964ff";
const NOTE_B: &str = "7a9619ff-8b86-d011-b42d-00cf4fc964ff";

async fn ready_world(pool: PgPool) -> (World, i64) {
    let w = World::new(pool, utc(2026, 12, 9, 12, 0)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    let id = w.target("AAA").await.id;
    w.make_ready(id).await;
    (w, id)
}

#[sqlx::test]
async fn a_note_after_the_kernel_was_accepted_triggers_a_new_preparation(pool: PgPool) {
    let (w, id) = ready_world(pool).await;
    w.clock.advance(ChronoDuration::minutes(5));

    let stored = w
        .notes
        .handle(&note_payload(
            "AAA",
            "XSTO",
            "",
            "order intake matters most this quarter",
            NOTE_A,
        ))
        .await
        .expect("handled");
    assert_eq!(stored, NoteOutcome::Stored);
    tokio::time::timeout(Duration::from_millis(200), w.wake.notified())
        .await
        .expect("the scheduler is woken");

    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!((report.reprepared, report.started), (1, 1));
    let launched = w.launcher.launched();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].notes.len(), 1);
    assert_eq!(
        launched[0].notes[0].text,
        "order intake matters most this quarter"
    );
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Preparing
    );
    assert!(
        w.store.accepted_kernel(id).await.expect("query").is_some(),
        "the old kernel stays until the new one is accepted"
    );
}

#[sqlx::test]
async fn a_note_older_than_the_kernel_does_not(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 11, 0)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.scheduler.pass().await.expect("pass");
    w.notes
        .handle(&note_payload("AAA", "XSTO", "", "first", NOTE_A))
        .await
        .expect("handled");
    w.clock.advance(ChronoDuration::minutes(5));
    let id = w.target("AAA").await.id;
    w.make_ready(id).await;

    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!((report.reprepared, report.started), (0, 0));
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Ready
    );
}

#[sqlx::test]
async fn a_note_inside_the_minimum_lead_does_not_trigger_a_preparation(pool: PgPool) {
    let (w, _) = ready_world(pool).await;
    w.clock.set(utc(2026, 12, 10, 6, 30));
    w.notes
        .handle(&note_payload("AAA", "XSTO", "", "too late", NOTE_A))
        .await
        .expect("handled");
    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!((report.reprepared, report.started), (0, 0));
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Ready
    );
}

#[sqlx::test]
async fn a_note_for_an_unregistered_company_is_rejected(pool: PgPool) {
    let (w, _) = ready_world(pool).await;
    let outcome = w
        .notes
        .handle(&note_payload("ZZZ", "XSTO", "", "who?", NOTE_A))
        .await
        .expect("handled");
    assert_eq!(outcome, NoteOutcome::RejectedUnregistered);
    assert_eq!(w.note_count().await, 0);
    let wrong_exchange = w
        .notes
        .handle(&note_payload("AAA", "XHEL", "", "wrong exchange", NOTE_B))
        .await
        .expect("handled");
    assert_eq!(wrong_exchange, NoteOutcome::RejectedUnregistered);
    assert_eq!(w.note_count().await, 0);
}

#[sqlx::test]
async fn the_same_note_delivered_twice_is_one_row(pool: PgPool) {
    let (w, _) = ready_world(pool).await;
    let payload = note_payload("AAA", "XSTO", "", "once", NOTE_A);
    assert_eq!(
        w.notes.handle(&payload).await.expect("handled"),
        NoteOutcome::Stored
    );
    assert_eq!(
        w.notes.handle(&payload).await.expect("handled"),
        NoteOutcome::Duplicate
    );
    assert_eq!(w.note_count().await, 1);
    let other = note_payload("AAA", "XSTO", "", "once more", NOTE_B);
    assert_eq!(
        w.notes.handle(&other).await.expect("handled"),
        NoteOutcome::Stored
    );
    assert_eq!(w.note_count().await, 2);
}

#[sqlx::test]
async fn oversized_and_malformed_notes_are_rejected(pool: PgPool) {
    let (w, _) = ready_world(pool).await;
    let at_limit = "x".repeat(4096);
    assert_eq!(
        w.notes
            .handle(&note_payload("AAA", "XSTO", "", &at_limit, NOTE_A))
            .await
            .expect("handled"),
        NoteOutcome::Stored
    );
    let over = "x".repeat(4097);
    assert_eq!(
        w.notes
            .handle(&note_payload("AAA", "XSTO", "", &over, NOTE_B))
            .await
            .expect("handled"),
        NoteOutcome::RejectedTooLarge
    );
    let bad_id = w
        .notes
        .handle(&note_payload("AAA", "XSTO", "", "text", "not-a-uuid"))
        .await
        .expect("handled");
    assert!(matches!(bad_id, NoteOutcome::RejectedInvalid(_)));
    let empty = w
        .notes
        .handle(&note_payload("AAA", "XSTO", "", "   ", NOTE_B))
        .await
        .expect("handled");
    assert!(matches!(empty, NoteOutcome::RejectedInvalid(_)));
    let no_ticker = w
        .notes
        .handle(&note_payload("", "XSTO", "", "text", NOTE_B))
        .await
        .expect("handled");
    assert!(matches!(no_ticker, NoteOutcome::RejectedInvalid(_)));
    let garbage = w
        .notes
        .handle(&[0xff, 0xff, 0xff, 0xff])
        .await
        .expect("handled");
    assert!(matches!(garbage, NoteOutcome::RejectedInvalid(_)));
    assert_eq!(w.note_count().await, 1);
}

#[sqlx::test]
async fn a_note_for_a_failed_target_gives_one_more_attempt(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 9, 19, 0)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule("AAA", "XSTO", date(2026, 12, 10), Some(time(8, 0)))
        .await;
    w.launcher.fail_with(Some("boom"));
    for minutes in [0, 5, 10] {
        w.clock.set(utc(2026, 12, 9, 19, minutes));
        w.scheduler.pass().await.expect("pass");
    }
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Failed
    );
    assert_eq!(w.launcher.launched().len(), 3);

    w.launcher.fail_with(None);
    w.clock.set(utc(2026, 12, 9, 20, 0));
    w.notes
        .handle(&note_payload(
            "AAA",
            "XSTO",
            "",
            "try again with this",
            NOTE_A,
        ))
        .await
        .expect("handled");
    let report = w.scheduler.pass().await.expect("pass");
    assert_eq!((report.retried, report.started), (1, 1));
    assert_eq!(w.launcher.launched().len(), 4);
    assert_eq!(w.launcher.launched()[3].attempt, 3);

    w.scheduler
        .finish_run(
            w.launcher.launched()[3].run_id,
            analyst::RunResult::Failed,
            None,
        )
        .await
        .expect("finish");
    assert_eq!(
        w.target("AAA").await.status().expect("status"),
        TargetStatus::Failed,
        "one more attempt, not more"
    );
    w.clock.set(utc(2026, 12, 9, 20, 30));
    assert_eq!(
        w.scheduler.pass().await.expect("pass").retried,
        0,
        "the same note does not trigger twice"
    );
}

#[sqlx::test]
async fn a_note_without_a_period_applies_to_the_next_report_only(pool: PgPool) {
    let w = World::new(pool, utc(2026, 12, 4, 12, 0)).await;
    w.add_company("AAA", "XSTO").await;
    w.schedule_with(
        "AAA",
        "XSTO",
        date(2026, 12, 10),
        Some(time(8, 0)),
        Some("Q4 2026"),
        "mfn",
    )
    .await;
    w.scheduler.pass().await.expect("pass");
    w.notes
        .handle(&note_payload("AAA", "XSTO", "", "for the next one", NOTE_A))
        .await
        .expect("handled");
    w.notes
        .handle(&note_payload("AAA", "XSTO", "Q4 2026", "for Q4", NOTE_B))
        .await
        .expect("handled");
    w.notes
        .handle(&note_payload(
            "AAA",
            "XSTO",
            "Q1 2027",
            "for Q1 2027",
            "8b9619ff-8b86-d011-b42d-00cf4fc964ff",
        ))
        .await
        .expect("handled");
    w.clock.set(utc(2027, 3, 4, 12, 0));
    w.schedule_with(
        "AAA",
        "XSTO",
        date(2027, 3, 10),
        Some(time(8, 0)),
        Some("Q1 2027"),
        "mfn",
    )
    .await;
    w.scheduler.pass().await.expect("pass");

    let first = w
        .store
        .target(1)
        .await
        .expect("query")
        .expect("first target");
    let second = w.target("AAA").await;
    assert_ne!(first.id, second.id);
    let texts =
        |notes: Vec<analyst::store::Note>| notes.into_iter().map(|n| n.text).collect::<Vec<_>>();
    assert_eq!(
        texts(w.store.notes_for(&first).await.expect("notes")),
        vec!["for the next one", "for Q4"]
    );
    assert_eq!(
        texts(w.store.notes_for(&second).await.expect("notes")),
        vec!["for Q1 2027"]
    );
}
