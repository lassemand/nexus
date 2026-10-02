//! Dispatcher behaviour that manual testing does not reveal: two runs in one
//! session, the concurrency cap being exceeded, and events lost across a
//! restart. Measured from the fake claude's log rather than from internals.

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::{git, linear_event, max_overlap, Harness};
use conductor::registry::{GroupKey, GroupState, RunResult};
use conductor::EventSink;
use sqlx::PgPool;

/// Generous enough for slow CI, short enough to fail fast.
const PATIENCE: Duration = Duration::from_secs(10);

fn group(name: &str) -> GroupKey {
    GroupKey::Named(name.into())
}

// ── serialisation within a group ─────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn runs_in_one_group_never_overlap_and_keep_order(pool: PgPool) {
    let h = Harness::new(pool).with_fake(80, 0, None, false);
    let (_d, sink) = h.start();

    for n in 1..=3 {
        sink.submit(linear_event(
            &format!("NEX-{n}"),
            "serial",
            Some(&format!("d{n}")),
        ));
    }

    assert!(
        h.wait_for_runs(3, PATIENCE).await,
        "expected 3 runs, saw {:?}",
        h.starts().len()
    );

    // The whole point: one session is never driven by two processes at once.
    assert_eq!(
        max_overlap(&h.runs()),
        1,
        "runs in a group must not overlap"
    );

    let starts = h.starts();
    let issues: Vec<String> = starts
        .iter()
        .map(|r| {
            let prompt = r.argv.last().cloned().unwrap_or_default();
            prompt
        })
        .collect();
    assert!(issues[0].contains("NEX-1"), "first run was {:?}", issues[0]);
    assert!(issues[1].contains("NEX-2"));
    assert!(issues[2].contains("NEX-3"));

    // First run starts the session, the rest resume the same one.
    let (first_id, first_resumed) = starts[0].session().expect("session arg");
    assert!(!first_resumed, "the first run must not resume");
    for later in &starts[1..] {
        let (id, resumed) = later.session().expect("session arg");
        assert!(resumed, "later runs must resume");
        assert_eq!(id, first_id, "the session id must be stable across runs");
    }
}

// ── the global cap ───────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn concurrency_is_capped_across_groups(pool: PgPool) {
    let h = Harness::new(pool).with_fake(300, 0, None, false);
    let (_d, sink) = h.start();

    for n in 0..8 {
        sink.submit(linear_event(
            &format!("NEX-{n}"),
            &format!("grp-{n}"),
            Some(&format!("d{n}")),
        ));
    }

    assert!(
        h.wait_for_runs(8, Duration::from_secs(30)).await,
        "all 8 groups should finish"
    );
    assert!(
        max_overlap(&h.runs()) <= 5,
        "cap is 5, observed {}",
        max_overlap(&h.runs())
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_lower_cap_is_respected(pool: PgPool) {
    let h = Harness::new(pool)
        .with_max_sessions(2)
        .with_fake(250, 0, None, false);
    let (_d, sink) = h.start();

    for n in 0..5 {
        sink.submit(linear_event(
            &format!("NEX-{n}"),
            &format!("grp-{n}"),
            Some(&format!("d{n}")),
        ));
    }

    assert!(h.wait_for_runs(5, Duration::from_secs(30)).await);
    assert!(
        max_overlap(&h.runs()) <= 2,
        "observed {}",
        max_overlap(&h.runs())
    );
}

// ── idempotency ──────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_repeated_delivery_runs_once(pool: PgPool) {
    let h = Harness::new(pool).with_fake(50, 0, None, false);
    let (_d, sink) = h.start();

    sink.submit(linear_event("NEX-1", "dedup", Some("same")));
    sink.submit(linear_event("NEX-1", "dedup", Some("same")));

    assert!(h.wait_for_runs(1, PATIENCE).await);
    // Give any second run time to appear before concluding it did not.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        h.starts().len(),
        1,
        "a duplicate delivery must not run twice"
    );
}

// ── worktrees ────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn each_group_runs_in_its_own_worktree(pool: PgPool) {
    let h = Harness::new(pool).with_fake(50, 0, None, false);
    let (_d, sink) = h.start();

    sink.submit(linear_event("NEX-1", "alpha", Some("d1")));
    sink.submit(linear_event("NEX-2", "beta", Some("d2")));
    assert!(h.wait_for_runs(2, PATIENCE).await);

    let starts = h.starts();
    let mut dirs: Vec<String> = starts.iter().map(|r| r.cwd.clone()).collect();
    dirs.sort();
    dirs.dedup();
    assert_eq!(dirs.len(), 2, "two groups must not share a checkout");

    for run in &starts {
        // Each worktree needs its own build cache, or parallel runs serialise on
        // cargo's lock.
        let expected = format!("{}/target", run.cwd);
        assert_eq!(run.env_cargo_target_dir.as_deref(), Some(expected.as_str()));

        let branch = git(
            std::path::Path::new(&run.cwd),
            &["branch", "--show-current"],
        );
        assert!(
            branch.trim().starts_with("agent/"),
            "on branch {}",
            branch.trim()
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn an_existing_agent_branch_is_checked_out_not_recreated(pool: PgPool) {
    let h = Harness::new(pool).with_fake(50, 0, None, false);
    // The branch exists in the repo but has no worktree.
    git(&h.repo, &["branch", "agent/reuse"]);

    let (_d, sink) = h.start();
    sink.submit(linear_event("NEX-1", "reuse", Some("d1")));

    assert!(
        h.wait_for_runs(1, PATIENCE).await,
        "the run should not fail on an existing branch"
    );
    let run = &h.starts()[0];
    let branch = git(
        std::path::Path::new(&run.cwd),
        &["branch", "--show-current"],
    );
    assert_eq!(branch.trim(), "agent/reuse");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_foreign_directory_at_the_worktree_path_is_left_alone(pool: PgPool) {
    let h = Harness::new(pool).with_fake(50, 0, None, false);
    // Something that is not a worktree, holding work that must not be destroyed.
    let squatter = h.worktrees.join("squat");
    std::fs::create_dir_all(&squatter).expect("mkdir");
    std::fs::write(squatter.join("precious.txt"), "do not delete").expect("write");

    let (_d, sink) = h.start();
    sink.submit(linear_event("NEX-1", "squat", Some("d1")));

    // The run cannot proceed, but the group must not be left wedged.
    let key = group("squat");
    // The run fails while preparing the worktree, so nothing is ever spawned.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(h.starts().is_empty(), "nothing should have been spawned");
    assert_eq!(
        std::fs::read_to_string(squatter.join("precious.txt")).expect("still there"),
        "do not delete",
        "the directory's contents must be untouched"
    );
    let group = h.group(&key).await.expect("group exists");
    assert_eq!(
        group.state,
        GroupState::Idle,
        "the group must not stay Running"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn branches_created_by_the_agent_are_recorded(pool: PgPool) {
    // Routing a later PR comment back here depends on having seen the branch.
    let h = Harness::new(pool).with_fake(50, 0, Some("git checkout -b feature/x"), false);
    let (_d, sink) = h.start();
    sink.submit(linear_event("NEX-1", "branches", Some("d1")));

    assert!(h.wait_for_runs(1, PATIENCE).await);
    let key = group("branches");

    // Branches are recorded when the run completes, so poll for it.
    let mut branches = Default::default();
    for _ in 0..100 {
        if let Some(g) = h.group(&key).await {
            if g.branches.iter().any(|b| b == "feature/x") {
                branches = g.branches;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        branches.contains("feature/x"),
        "the agent's own branch must be recorded for later PR routing; saw {branches:?}"
    );
    assert!(
        branches.iter().any(|b| b.starts_with("agent/")),
        "the branch the worktree started on must be kept too; saw {branches:?}"
    );
}

// ── failure and timeout ──────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_failing_run_is_recorded_and_not_retried(pool: PgPool) {
    let h = Harness::new(pool).with_fake(50, 1, None, false);
    let (_d, sink) = h.start();
    sink.submit(linear_event("NEX-1", "failing", Some("d1")));

    assert!(h.wait_for_runs(1, PATIENCE).await);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(h.starts().len(), 1, "a failed run must not be retried");

    let group = h.group(&group("failing")).await.expect("group");
    let outcome = group.last_run.expect("an outcome");
    assert_eq!(outcome.result, RunResult::Exited { code: 1 });
    assert_eq!(group.state, GroupState::Idle);

    // And the group is not poisoned: its next event still runs.
    sink.submit(linear_event("NEX-2", "failing", Some("d2")));
    assert!(
        h.wait_for_runs(2, PATIENCE).await,
        "the next event should still run"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_run_that_ignores_sigterm_is_killed_and_recorded_as_timed_out(pool: PgPool) {
    let h = Harness::new(pool)
        .with_run_timeout(Duration::from_millis(400))
        // Sleeps far past the timeout and refuses to stop politely.
        .with_fake(10_000, 0, None, true);
    let (_d, sink) = h.start();
    sink.submit(linear_event("NEX-1", "timeout", Some("d1")));

    let key = group("timeout");

    // Poll the registry until the outcome lands.
    let mut outcome = None;
    for _ in 0..150 {
        if let Some(g) = h.group(&key).await {
            if let Some(o) = g.last_run.clone() {
                outcome = Some(o);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let outcome = outcome.expect("the run should have been stopped and recorded");
    assert_eq!(outcome.result, RunResult::TimedOut);
}

// ── state changed from outside the process ───────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn an_attached_group_waits_until_it_is_idle_again(pool: PgPool) {
    let h = Harness::new(pool).with_fake(50, 0, None, false);
    let key = group("attached");

    // Create the group and mark it Attached, as the CLI would from another
    // process, using this process's own pid so it looks alive.
    {
        let mut txn = h.registry.begin().await.expect("begin");
        txn.session_for_dispatch(&key).await.expect("create");
        txn.commit().await.expect("commit");
    }
    sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
        .bind(key.slug())
        .bind(sqlx::types::Json(GroupState::Attached {
            pid: std::process::id(),
            // Must be this process's real start time: on Linux the registry
            // verifies it against /proc, and a fabricated value would be read as
            // a recycled pid, resetting the group and defeating the test.
            started_at: live_process_start(std::process::id()),
        }))
        .execute(&h.pool)
        .await
        .expect("set attached");

    let (_d, sink) = h.start();
    sink.submit(linear_event("NEX-1", "attached", Some("d1")));

    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        h.starts().is_empty(),
        "an attached group must not be started"
    );

    // Release it; the periodic re-scan should pick the event up without being told.
    sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
        .bind(key.slug())
        .bind(sqlx::types::Json(GroupState::Idle))
        .execute(&h.pool)
        .await
        .expect("set idle");

    assert!(
        h.wait_for_runs(1, PATIENCE).await,
        "the re-scan should have found the released group"
    );
}

// ── shutdown and recovery ────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn shutdown_refuses_webhooks_and_stops_runs_without_requeueing(pool: PgPool) {
    let h = Harness::new(pool)
        .with_shutdown_grace(Duration::from_millis(200))
        .with_fake(10_000, 0, None, true);
    let (dispatcher, sink) = h.start();
    sink.submit(linear_event("NEX-1", "draining", Some("d1")));

    // Wait until the run is actually in flight.
    assert!(
        h.wait_for(PATIENCE, |hh| !hh.starts().is_empty()).await,
        "a run should have started"
    );

    Arc::clone(&dispatcher).shutdown().await;
    assert!(
        h.shutting_down.load(Ordering::SeqCst),
        "draining must be visible to the HTTP layer so it can answer 503"
    );

    let group = h.group(&group("draining")).await.expect("group");
    assert!(
        group.pending.is_empty(),
        "an interrupted event must not be re-queued; re-running it could open a duplicate PR"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_new_dispatcher_picks_up_events_left_by_a_crashed_one(pool: PgPool) {
    let h = Harness::new(pool).with_fake(50, 0, None, false);

    // Queue work without ever running a scheduler: the shape a crash leaves.
    for n in 1..=3 {
        let key = group(&format!("crash-{n}"));
        let mut txn = h.registry.begin().await.expect("begin");
        txn.session_for_dispatch(&key).await.expect("create");
        txn.enqueue(
            &key,
            &linear_event(&format!("NEX-{n}"), &format!("crash-{n}"), None),
        )
        .await
        .expect("enqueue");
        txn.commit().await.expect("commit");
    }

    let (_d, _sink) = h.start();
    assert!(
        h.wait_for_runs(3, Duration::from_secs(20)).await,
        "a restart must not lose accepted events"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_group_left_running_on_a_dead_pid_is_recovered(pool: PgPool) {
    let h = Harness::new(pool).with_fake(50, 0, None, false);
    let key = group("dead");

    // A process that has certainly exited.
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn");
    let dead_pid = child.id();
    child.wait().expect("wait");

    {
        let mut txn = h.registry.begin().await.expect("begin");
        txn.session_for_dispatch(&key).await.expect("create");
        txn.enqueue(&key, &linear_event("NEX-1", "dead", None))
            .await
            .expect("enqueue");
        txn.commit().await.expect("commit");
    }
    sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
        .bind(key.slug())
        .bind(sqlx::types::Json(GroupState::Running {
            pid: dead_pid,
            started_at: chrono::Utc::now(),
        }))
        .execute(&h.pool)
        .await
        .expect("set running");

    let (_d, _sink) = h.start();
    assert!(
        h.wait_for_runs(1, PATIENCE).await,
        "the dead run should be reset and its event run"
    );
}

/// Wall-clock start time of a running process, as the registry computes it.
///
/// Duplicated in the test rather than exposed from the crate: the point is to
/// build a record the production code will *accept*, so deriving it the same way
/// from the same source is the honest way to do it.
#[cfg(target_os = "linux")]
fn live_process_start(pid: u32) -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("/proc stat");
    // Field 2 is the command and may contain spaces and parens, so start after
    // the last ')'. The remainder begins at field 3, so field 22 is index 19.
    let ticks: u64 = stat
        .rsplit_once(')')
        .expect("comm")
        .1
        .split_whitespace()
        .nth(19)
        .expect("starttime")
        .parse()
        .expect("numeric");
    let hz = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .expect("sysconf")
        .expect("clk_tck") as u64;
    let btime: i64 = std::fs::read_to_string("/proc/stat")
        .expect("/proc/stat")
        .lines()
        .find_map(|l| l.strip_prefix("btime "))
        .expect("btime")
        .trim()
        .parse()
        .expect("numeric");
    chrono::Utc
        .timestamp_opt(btime + (ticks / hz) as i64, 0)
        .single()
        .expect("valid time")
}

/// On macOS there is no `/proc`, so liveness is `kill(pid, 0)` alone and any
/// recorded start time is accepted.
#[cfg(not(target_os = "linux"))]
fn live_process_start(_pid: u32) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_group_running_on_a_live_process_is_left_alone(pool: PgPool) {
    let h = Harness::new(pool).with_fake(50, 0, None, false);
    let key = group("live");

    // Something genuinely running, with the start time the registry will verify.
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 5"])
        .spawn()
        .expect("spawn");
    let live_pid = child.id();
    let started_at = live_process_start(live_pid);

    {
        let mut txn = h.registry.begin().await.expect("begin");
        txn.session_for_dispatch(&key).await.expect("create");
        txn.enqueue(&key, &linear_event("NEX-1", "live", None))
            .await
            .expect("enqueue");
        txn.commit().await.expect("commit");
    }
    sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
        .bind(key.slug())
        .bind(sqlx::types::Json(GroupState::Running {
            pid: live_pid,
            started_at,
        }))
        .execute(&h.pool)
        .await
        .expect("set running");

    let (_d, _sink) = h.start();
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Starting a second run here would mean two processes driving one session.
    assert!(
        h.starts().is_empty(),
        "a group whose process is still alive must not be started again"
    );
    let group = h.group(&key).await.expect("group");
    assert!(
        matches!(group.state, GroupState::Running { .. }),
        "state was {:?}",
        group.state
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[sqlx::test(migrations = "./migrations")]
async fn a_webhook_posted_to_the_router_reaches_a_run(pool: PgPool) {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let h = Harness::new(pool).with_fake(50, 0, None, false);
    let (_dispatcher, sink) = h.start();

    let router = conductor::http::router(conductor::http::AppState {
        sink: Arc::new(sink),
        github_webhook_secret: None,
        watched_github_users: vec!["lassemand".into()],
        shutting_down: Arc::clone(&h.shutting_down),
    });

    // A real transition into Todo, as Linear would send it.
    let payload = serde_json::json!({
        "action": "create",
        "type": "Issue",
        "data": {
            "identifier": "NEX-500",
            "title": "end to end",
            "description": "d",
            "state": { "name": "Todo" },
            "labels": [{ "name": "session:e2e" }]
        }
    });

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("content-type", "application/json")
                .header("Linear-Delivery", "e2e-1")
                .body(Body::from(payload.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        h.wait_for_runs(1, PATIENCE).await,
        "a posted webhook should end in a run"
    );
    let run = &h.starts()[0];
    assert!(
        run.argv.last().expect("prompt").contains("NEX-500"),
        "the prompt should carry the issue"
    );
}
