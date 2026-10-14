//! End-to-end behaviour of automatic closing and of session reuse within a set.
//!
//! Driven with an in-memory [`FakeSource`] standing in for Linear and the fake
//! `claude` from `tests/fixtures`, so nothing here talks to the network and no
//! test waits on a real timer: the sweep is disabled (`close_sweep: None`) and
//! passes are driven explicitly through [`Dispatcher::close_pass`], while the
//! webhook-triggered path runs on the scheduler's 100ms rescan.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use common::{git, max_overlap, Harness};
use conductor::dispatcher::{DispatchSink, Dispatcher, PerPullRequestResolver};
use conductor::linear::{ClosedIssue, ClosedKind, LinearIssue};
use conductor::linear_api::{IssueSetSource, LinearApiError, SetIssue};
use conductor::registry::{recorded_start_time, Clock, GroupKey, GroupState, Registry};
use conductor::resolve::fallback_key;
use conductor::sessions::SessionsCommand;
use conductor::{EventSink, InboundEvent};
use sqlx::PgPool;

/// Ceiling for work the test expects to happen; a failing assertion does not wait it out.
const PATIENCE: Duration = Duration::from_secs(30);

// ── fakes ────────────────────────────────────────────────────────────────────

enum Answer {
    Issues(Vec<(String, String)>),
    Fail,
}

/// Linear stand-in whose answer a test can change between steps.
struct FakeSource {
    answer: Mutex<Answer>,
    asked: Mutex<Vec<String>>,
    calls: AtomicUsize,
}

impl FakeSource {
    fn new(issues: &[(&str, &str)]) -> Arc<Self> {
        let source = Arc::new(FakeSource {
            answer: Mutex::new(Answer::Fail),
            asked: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        source.set(issues);
        source
    }

    fn set(&self, issues: &[(&str, &str)]) {
        *self.answer.lock().unwrap() = Answer::Issues(
            issues
                .iter()
                .map(|(i, s)| (i.to_string(), s.to_string()))
                .collect(),
        );
    }

    fn fail(&self) {
        *self.answer.lock().unwrap() = Answer::Fail;
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl IssueSetSource for FakeSource {
    async fn issues_in_set(&self, key: &GroupKey) -> Result<Vec<SetIssue>, LinearApiError> {
        self.asked.lock().unwrap().push(key.slug());
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &*self.answer.lock().unwrap() {
            Answer::Issues(v) => Ok(v
                .iter()
                .map(|(i, s)| SetIssue {
                    identifier: i.clone(),
                    state_type: s.clone(),
                })
                .collect()),
            Answer::Fail => Err(LinearApiError::Status(500)),
        }
    }
}

/// Clock a test moves by hand, to place two events days apart without waiting.
struct TestClock(Mutex<DateTime<Utc>>);

impl TestClock {
    fn new() -> Arc<Self> {
        Arc::new(TestClock(Mutex::new(Utc::now())))
    }
    fn advance(&self, by: ChronoDuration) {
        *self.0.lock().unwrap() += by;
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// Collects log output from the current thread, which is where `#[sqlx::test]`
/// runs the dispatcher's spawned tasks too.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(self.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    fn contains(&self, needle: &str) -> bool {
        String::from_utf8_lossy(&self.0.lock().unwrap()).contains(needle)
    }
}

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn todo(identifier: &str, labels: &[&str], delivery: &str) -> InboundEvent {
    InboundEvent::LinearIssueTodo {
        issue: LinearIssue {
            identifier: identifier.into(),
            title: "t".into(),
            description: "d".into(),
            labels: labels.iter().map(|l| format!("session:{l}")).collect(),
            session_labels: labels.iter().map(|l| l.to_string()).collect(),
        },
        delivery_id: Some(delivery.into()),
    }
}

fn closed(identifier: &str, labels: &[&str], kind: ClosedKind, delivery: &str) -> InboundEvent {
    InboundEvent::LinearIssueClosed {
        issue: ClosedIssue {
            identifier: identifier.into(),
            session_labels: labels.iter().map(|l| l.to_string()).collect(),
            kind,
        },
        delivery_id: Some(delivery.into()),
    }
}

fn named(name: &str) -> GroupKey {
    GroupKey::Named(name.into())
}

fn harness(pool: PgPool, sleep_ms: u64) -> Harness {
    Harness::new(pool).with_fake(sleep_ms, 0, None, false)
}

/// Submits a Todo event and waits until its run is recorded and the group is idle.
async fn run_todo(
    h: &Harness,
    sink: &DispatchSink,
    key: &GroupKey,
    identifier: &str,
    labels: &[&str],
    delivery: &str,
) {
    sink.submit(todo(identifier, labels, delivery));
    let id = identifier.to_string();
    assert!(
        h.wait_for_group(key, PATIENCE, |g| {
            matches!(g.state, GroupState::Ready { .. }) && g.issues.contains(&id)
        })
        .await,
        "{identifier} never finished running"
    );
}

/// Waits until the fake has been asked at least `n` times in total.
async fn wait_calls(source: &FakeSource, n: usize) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while source.calls() < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Linear was asked {} time(s), expected {n}",
            source.calls()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until a check has run and withdrawn its request, then asserts the group is still there.
async fn assert_stays_open(h: &Harness, key: &GroupKey, source: &FakeSource, calls_before: usize) {
    wait_calls(source, calls_before + 1).await;
    assert!(
        h.wait_for_group(key, PATIENCE, |g| g.close_requested_at.is_none())
            .await,
        "request should be withdrawn after the check"
    );
    assert!(h.group(key).await.is_some(), "{} was closed", key.slug());
}

async fn wait_gone(h: &Harness, key: &GroupKey) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while h.group(key).await.is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} was never closed",
            key.slug()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn audit(h: &Harness) -> Vec<conductor::registry::ClosedGroup> {
    h.registry.recent_closed(100).await.expect("audit rows")
}

fn attached() -> GroupState {
    let pid = std::process::id();
    GroupState::Attached {
        pid,
        started_at: recorded_start_time(pid),
    }
}

// ── closing ──────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn set_closes_only_once_every_issue_is_complete(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed"), ("NEX-2", "backlog")]);
    let (_d, sink) = h.start_with_source(source.clone());
    let key = named("conductor-x");

    run_todo(&h, &sink, &key, "NEX-1", &["conductor-x"], "t1").await;
    sink.submit(closed(
        "NEX-1",
        &["conductor-x"],
        ClosedKind::Completed,
        "c1",
    ));
    assert_stays_open(&h, &key, &source, 0).await;

    // B joins the set, runs, and completes.
    run_todo(&h, &sink, &key, "NEX-2", &["conductor-x"], "t2").await;
    let worktree = h.group(&key).await.expect("group").worktree;
    assert!(worktree.exists());
    source.set(&[("NEX-1", "completed"), ("NEX-2", "completed")]);
    sink.submit(closed(
        "NEX-2",
        &["conductor-x"],
        ClosedKind::Completed,
        "c2",
    ));
    wait_gone(&h, &key).await;

    assert!(!worktree.exists(), "worktree directory should be gone");
    let rows = audit(&h).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].slug, "conductor-x");
    assert_eq!(rows[0].mode, "auto");
    assert_eq!(rows[0].issues, vec!["NEX-1", "NEX-2"]);
    assert!(rows[0].branches.contains(&"agent/conductor-x".to_string()));
    // Only the worktree and the row go; the branch is kept.
    assert!(!git(&h.repo, &["branch", "--list", "agent/conductor-x"])
        .trim()
        .is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn completed_and_canceled_closes(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed"), ("NEX-2", "canceled")]);
    let (_d, sink) = h.start_with_source(source);
    let key = named("mixed");

    run_todo(&h, &sink, &key, "NEX-1", &["mixed"], "t1").await;
    sink.submit(closed("NEX-1", &["mixed"], ClosedKind::Completed, "c1"));
    wait_gone(&h, &key).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn in_review_counts_as_open(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed"), ("NEX-2", "started")]);
    let (_d, sink) = h.start_with_source(source.clone());
    let key = named("review");

    run_todo(&h, &sink, &key, "NEX-1", &["review"], "t1").await;
    sink.submit(closed("NEX-1", &["review"], ClosedKind::Completed, "c1"));
    assert_stays_open(&h, &key, &source, 0).await;
    assert!(audit(&h).await.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn unlabelled_issue_is_a_set_of_one(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-7", "completed")]);
    let (_d, sink) = h.start_with_source(source);
    let key = GroupKey::Issue("NEX-7".into());
    assert_eq!(key.slug(), "linear-nex-7");

    run_todo(&h, &sink, &key, "NEX-7", &[], "t1").await;
    sink.submit(closed("NEX-7", &[], ClosedKind::Completed, "c1"));
    wait_gone(&h, &key).await;
    assert_eq!(audit(&h).await[0].slug, "linear-nex-7");
}

#[sqlx::test(migrations = "./migrations")]
async fn removing_the_last_open_issue_closes_the_set(pool: PgPool) {
    let h = harness(pool, 20);
    // NEX-2 was the open one; once removed Linear no longer returns it.
    let source = FakeSource::new(&[("NEX-1", "completed")]);
    let (_d, sink) = h.start_with_source(source);
    let key = named("removal");

    run_todo(&h, &sink, &key, "NEX-1", &["removal"], "t1").await;
    sink.submit(closed("NEX-2", &["removal"], ClosedKind::Removed, "c1"));
    wait_gone(&h, &key).await;
}

// ── never close on doubt ─────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn empty_answer_never_closes_and_warns(pool: PgPool) {
    let logs = LogCapture::default();
    let _guard = logs.install();
    let h = harness(pool, 20);
    let source = FakeSource::new(&[]);
    let (_d, sink) = h.start_with_source(source.clone());
    let key = named("empty");

    run_todo(&h, &sink, &key, "NEX-1", &["empty"], "t1").await;
    sink.submit(closed("NEX-1", &["empty"], ClosedKind::Completed, "c1"));
    assert_stays_open(&h, &key, &source, 0).await;
    assert!(
        logs.contains("Linear returned no issues"),
        "no warning logged"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn linear_error_never_closes_and_the_next_sweep_recovers(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[]);
    source.fail();
    let (d, sink) = h.start_with_source(source.clone());
    let key = named("flaky");

    run_todo(&h, &sink, &key, "NEX-1", &["flaky"], "t1").await;
    sink.submit(closed("NEX-1", &["flaky"], ClosedKind::Completed, "c1"));
    assert_stays_open(&h, &key, &source, 0).await;

    source.set(&[("NEX-1", "completed")]);
    d.close_pass(true).await;
    wait_gone(&h, &key).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn pull_request_groups_are_never_auto_closed(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed")]);
    let (d, _sink) = h.start_with_source(source.clone());
    let key = fallback_key("lassemand/nexus", 5);
    assert!(key.slug().starts_with("gh-"));
    h.registry
        .session_for_dispatch(&key)
        .await
        .expect("create group");

    d.close_pass(true).await;

    assert!(h.group(&key).await.is_some());
    assert!(
        source.asked().is_empty(),
        "asked about {:?}",
        source.asked()
    );
}

// ── busy groups ──────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn completion_during_a_run_closes_after_it_ends(pool: PgPool) {
    let h = harness(pool, 1500);
    let source = FakeSource::new(&[("NEX-1", "completed")]);
    let (_d, sink) = h.start_with_source(source.clone());
    let key = named("busy");

    sink.submit(todo("NEX-1", &["busy"], "t1"));
    assert!(h.wait_for(PATIENCE, |h| !h.starts().is_empty()).await);
    sink.submit(closed("NEX-1", &["busy"], ClosedKind::Completed, "c1"));

    // The paste is logged before the hook confirms it, so wait for the turn to
    // be Busy rather than assuming a fixed delay is enough under load.
    assert!(
        h.wait_for_state(&key, PATIENCE, |s| matches!(s, GroupState::Busy { .. }))
            .await
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(matches!(
        h.state_of(&key).await,
        Some(GroupState::Busy { .. })
    ));
    assert_eq!(source.calls(), 0, "no check while the run is going");

    wait_gone(&h, &key).await;
    assert!(
        h.runs().iter().any(|r| r.event == "end"),
        "closed before the run ended"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn completion_while_attached_closes_after_release(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed")]);
    let (_d, sink) = h.start_with_source(source.clone());
    let key = named("attach");

    run_todo(&h, &sink, &key, "NEX-1", &["attach"], "t1").await;
    h.fake.set_clients(&key.slug(), &["client-1"]);
    sink.submit(closed("NEX-1", &["attach"], ClosedKind::Completed, "c1"));
    assert!(
        h.wait_for_group(&key, PATIENCE, |g| g.close_requested_at.is_some())
            .await
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(h.group(&key).await.is_some());
    assert_eq!(source.calls(), 0);

    h.fake.set_clients(&key.slug(), &[]);
    wait_gone(&h, &key).await;
    assert!(
        h.fake.killed.lock().unwrap().contains(&key.slug()),
        "closing kills the tmux session"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn pending_work_runs_before_the_group_closes(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed"), ("NEX-2", "completed")]);
    let (_d, sink) = h.start_with_source(source.clone());
    let key = named("pending");

    run_todo(&h, &sink, &key, "NEX-1", &["pending"], "t1").await;
    // Attached keeps the scheduler from dispatching while work is queued.
    h.set_state(&key, &attached()).await;
    h.enqueue(&key, &todo("NEX-2", &["pending"], "t2")).await;
    sink.submit(closed("NEX-1", &["pending"], ClosedKind::Completed, "c1"));
    assert!(
        h.wait_for_group(&key, PATIENCE, |g| g.close_requested_at.is_some())
            .await
    );

    h.set_state(
        &key,
        &GroupState::Ready {
            pane_pid: 1,
            since: chrono::Utc::now(),
        },
    )
    .await;
    wait_gone(&h, &key).await;

    assert_eq!(
        h.starts().len(),
        2,
        "the queued event must run before the close"
    );
    assert_eq!(audit(&h).await[0].issues, vec!["NEX-1", "NEX-2"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_reopened_task_is_noticed_at_decision_time(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed"), ("NEX-2", "completed")]);
    let (_d, sink) = h.start_with_source(source.clone());
    let key = named("reopen");

    run_todo(&h, &sink, &key, "NEX-1", &["reopen"], "t1").await;
    h.set_state(&key, &attached()).await;
    sink.submit(closed("NEX-1", &["reopen"], ClosedKind::Completed, "c1"));
    assert!(
        h.wait_for_group(&key, PATIENCE, |g| g.close_requested_at.is_some())
            .await
    );

    // Between the webhook and the pass, someone moves a task back.
    source.set(&[("NEX-1", "completed"), ("NEX-2", "unstarted")]);
    h.set_state(&key, &GroupState::Idle).await;

    assert_stays_open(&h, &key, &source, 0).await;
    assert!(audit(&h).await.is_empty());
}

// ── dirty worktree ───────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn dirty_worktree_blocks_until_closed_by_hand(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed")]);
    let (d, sink) = h.start_with_source(source.clone());
    let key = named("dirty");

    run_todo(&h, &sink, &key, "NEX-1", &["dirty"], "t1").await;
    let worktree = h.group(&key).await.expect("group").worktree;
    let stray = worktree.join("uncommitted.txt");
    std::fs::write(&stray, "work in progress").unwrap();

    sink.submit(closed("NEX-1", &["dirty"], ClosedKind::Completed, "c1"));
    assert!(
        h.wait_for_group(&key, PATIENCE, |g| g.close_blocked.is_some())
            .await,
        "close should be blocked"
    );
    let group = h.group(&key).await.unwrap();
    assert!(group.close_requested_at.is_none());
    assert!(stray.exists(), "the file must survive");

    // Blocked groups are left alone, even by a full sweep.
    let calls = source.calls();
    d.close_pass(true).await;
    assert_eq!(source.calls(), calls, "a blocked group is not re-checked");

    let (_, listing) = h
        .sessions(SessionsCommand::List {
            json: false,
            closed: false,
            limit: 20,
        })
        .await;
    assert!(listing.contains("close blocked:"), "{listing}");

    let (code, _) = h
        .sessions(SessionsCommand::Close {
            group: "dirty".into(),
            force: true,
        })
        .await;
    assert_eq!(code, 0);
    assert!(h.group(&key).await.is_none());
    let rows = audit(&h).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].mode, "manual");
}

// ── races ────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_todo_racing_a_close_is_never_lost(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-0", "completed")]);
    // No scheduler: queued events must stay queued so they can be inspected.
    let (d, sink, rx) = Dispatcher::new(
        h.registry.clone(),
        h.config.clone(),
        Arc::new(PerPullRequestResolver),
        Arc::clone(&h.shutting_down),
        Some(source),
        h.fake.clone(),
    );
    tokio::spawn(Arc::clone(&d).run_ingest(rx));

    let (mut kept, mut replaced) = (0, 0);
    for i in 0..50 {
        let name = format!("race-{i}");
        let key = named(&name);
        let identifier = format!("NEX-{}", 100 + i);
        // A started session, as after a real run: ingest only keeps the id of
        // those, and mints a new one for a session that never began.
        let original = {
            let mut txn = h.registry.begin().await.expect("begin");
            let id = txn
                .session_for_dispatch(&key)
                .await
                .expect("create group")
                .0;
            txn.mark_session_started(&key).await.expect("start");
            txn.commit().await.expect("commit");
            id
        };

        let closer = {
            let d = Arc::clone(&d);
            async move {
                // Varies which side of the enqueue the close lands on.
                if i % 2 == 0 {
                    for _ in 0..(i % 5) {
                        tokio::task::yield_now().await;
                    }
                }
                d.close_pass(true).await;
            }
        };
        let enqueue = async {
            // Alternately delays the other side, so both orderings occur.
            if i % 2 == 1 {
                tokio::time::sleep(Duration::from_millis(i % 7)).await;
            }
            sink.submit(todo(&identifier, &[&name], &format!("race-t{i}")));
        };
        tokio::join!(closer, enqueue);

        // Wait for ingest to have placed the event somewhere.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let group = loop {
            if let Some(g) = h.group(&key).await {
                if !g.pending.is_empty() {
                    break g;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "iteration {i}: the event was lost"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(group.pending.len(), 1, "iteration {i}");

        let closed_rows: Vec<_> = audit(&h)
            .await
            .into_iter()
            .filter(|r| r.slug == name)
            .collect();
        match closed_rows.as_slice() {
            [] => {
                assert_eq!(
                    group.session_id, original,
                    "iteration {i}: group replaced without audit"
                );
                kept += 1;
            }
            [row] => {
                assert_eq!(row.session_id, original, "iteration {i}");
                assert_ne!(
                    group.session_id, original,
                    "iteration {i}: not a fresh group"
                );
                replaced += 1;
            }
            more => panic!("iteration {i}: closed {} times", more.len()),
        }
    }
    eprintln!("race outcomes: {kept} kept, {replaced} replaced");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_duplicate_completion_webhook_is_processed_once(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed"), ("NEX-2", "backlog")]);
    let (_d, sink) = h.start_with_source(source.clone());
    let key = named("dupe");
    run_todo(&h, &sink, &key, "NEX-1", &["dupe"], "t1").await;

    sink.submit(closed("NEX-1", &["dupe"], ClosedKind::Completed, "same"));
    sink.submit(closed("NEX-1", &["dupe"], ClosedKind::Completed, "same"));
    wait_calls(&source, 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(source.calls(), 1, "the retry must be dropped");

    // Control: a different delivery is a new request.
    sink.submit(closed("NEX-1", &["dupe"], ClosedKind::Completed, "other"));
    wait_calls(&source, 2).await;
}

// ── reuse ────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn tasks_three_days_apart_resume_one_session(pool: PgPool) {
    let clock = TestClock::new();
    let mut h = harness(pool.clone(), 20);
    h.registry = Registry::new(pool, clock.clone(), 7);
    let (_d, sink) = h.start_with_source(FakeSource::new(&[("NEX-1", "started")]));
    let key = named("conductor-x");

    run_todo(&h, &sink, &key, "NEX-1", &["conductor-x"], "t1").await;
    clock.advance(ChronoDuration::days(3));
    run_todo(&h, &sink, &key, "NEX-2", &["conductor-x"], "t2").await;

    let starts = h.starts();
    assert_eq!(starts.len(), 2);
    let (first, first_resumed) = starts[0].session().expect("first session");
    let (second, _) = starts[1].session().expect("second session");
    assert!(!first_resumed);
    // The second task is pasted into the set's live session rather than
    // starting a new one.
    assert_eq!(h.fake.started.lock().unwrap().len(), 1, "one tmux session");
    assert_eq!(first, second);
    assert_eq!(starts[0].cwd, starts[1].cwd, "same worktree");
}

#[sqlx::test(migrations = "./migrations")]
async fn different_labels_get_separate_parallel_groups(pool: PgPool) {
    let h = harness(pool, 600);
    let (_d, sink) = h.start_with_source(FakeSource::new(&[("NEX-1", "started")]));

    sink.submit(todo("NEX-1", &["alpha"], "t1"));
    sink.submit(todo("NEX-2", &["beta"], "t2"));
    assert!(h.wait_for_runs(2, PATIENCE).await);

    let starts = h.starts();
    assert_ne!(
        starts[0].session().unwrap().0,
        starts[1].session().unwrap().0
    );
    assert_ne!(starts[0].cwd, starts[1].cwd);
    assert_eq!(max_overlap(&h.runs()), 2, "different sets run in parallel");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_new_task_after_auto_close_starts_a_fresh_session(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed")]);
    let (_d, sink) = h.start_with_source(source.clone());
    let key = named("conductor-x");

    run_todo(&h, &sink, &key, "NEX-1", &["conductor-x"], "t1").await;
    sink.submit(closed(
        "NEX-1",
        &["conductor-x"],
        ClosedKind::Completed,
        "c1",
    ));
    wait_gone(&h, &key).await;

    source.set(&[("NEX-3", "started")]);
    run_todo(&h, &sink, &key, "NEX-3", &["conductor-x"], "t3").await;

    let starts = h.starts();
    let (first, _) = starts[0].session().unwrap();
    let (second, resumed) = starts[1].session().unwrap();
    assert!(!resumed, "a closed set must not be resumed");
    assert_ne!(first, second);
}

#[sqlx::test(migrations = "./migrations")]
async fn label_case_does_not_split_a_set(pool: PgPool) {
    let h = harness(pool, 20);
    let (_d, sink) = h.start_with_source(FakeSource::new(&[("NEX-1", "started")]));
    let key = named("conductor-x");

    run_todo(&h, &sink, &key, "NEX-1", &["Conductor-X"], "t1").await;
    run_todo(&h, &sink, &key, "NEX-2", &["conductor-x"], "t2").await;

    let starts = h.starts();
    let (first, _) = starts[0].session().unwrap();
    let (second, _) = starts[1].session().unwrap();
    assert_eq!(h.fake.started.lock().unwrap().len(), 1, "one tmux session");
    assert_eq!(first, second);
    assert_eq!(starts[0].cwd, starts[1].cwd);
}

// ── CLI visibility ───────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn list_shows_closing_and_the_audit_trail_newest_first(pool: PgPool) {
    let h = harness(pool, 20);
    let source = FakeSource::new(&[("NEX-1", "completed")]);
    let (_d, sink) = h.start_with_source(source.clone());

    // Two sets closed one after the other.
    let first = named("first-set");
    run_todo(&h, &sink, &first, "NEX-1", &["first-set"], "t1").await;
    sink.submit(closed("NEX-1", &["first-set"], ClosedKind::Completed, "c1"));
    wait_gone(&h, &first).await;
    let second = named("second-set");
    run_todo(&h, &sink, &second, "NEX-1", &["second-set"], "t2").await;
    sink.submit(closed(
        "NEX-1",
        &["second-set"],
        ClosedKind::Completed,
        "c2",
    ));
    wait_gone(&h, &second).await;

    let (code, listing) = h
        .sessions(SessionsCommand::List {
            json: false,
            closed: true,
            limit: 20,
        })
        .await;
    assert_eq!(code, 0);
    let newest = listing.find("second-set").expect("second-set listed");
    let oldest = listing.find("first-set").expect("first-set listed");
    assert!(newest < oldest, "newest first:\n{listing}");

    let (_, limited) = h
        .sessions(SessionsCommand::List {
            json: false,
            closed: true,
            limit: 1,
        })
        .await;
    assert!(limited.contains("second-set") && !limited.contains("first-set"));

    // A group awaiting its check shows as closing.
    let waiting = named("waiting");
    run_todo(&h, &sink, &waiting, "NEX-9", &["waiting"], "t3").await;
    h.set_state(&waiting, &attached()).await;
    sink.submit(closed("NEX-9", &["waiting"], ClosedKind::Completed, "c3"));
    assert!(
        h.wait_for_group(&waiting, PATIENCE, |g| g.close_requested_at.is_some())
            .await
    );
    let (_, listing) = h
        .sessions(SessionsCommand::List {
            json: false,
            closed: false,
            limit: 20,
        })
        .await;
    assert!(listing.contains("closing"), "{listing}");
}
