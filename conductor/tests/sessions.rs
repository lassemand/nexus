//! Tests for `conductor sessions`.
//!
//! The CLI's job is to take control of processes the server owns, so these
//! tests check the handover rather than the formatting: that a group is marked
//! attached for exactly as long as the interactive session lasts, that queued
//! events wait while it does, and that a group is always given back — including
//! when the connection drops and the terminal sends SIGHUP.
//!
//! Reuses NEX-129's fake claude and temp-repo harness, so no real Claude runs
//! and nothing touches the real repository.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{linear_event, Harness};
use conductor::dispatcher::{DispatchSink, Dispatcher, Signal};
use conductor::registry::{process_is_alive, recorded_start_time, GroupKey, GroupState, RunResult};
use conductor::sessions::SessionsCommand;
use conductor::EventSink;
use sqlx::PgPool;

/// Generous enough for a loaded CI box, short enough to fail fast.
const PATIENCE: Duration = Duration::from_secs(10);

fn named(group: &str) -> GroupKey {
    GroupKey::Named(group.into())
}

/// A process the test does not own, plus the record saying a group owns it.
///
/// Deliberately orphaned via `sh`, which exits immediately and leaves `sleep`
/// reparented: a direct child would stay a zombie after being killed until this
/// process reaped it, and a zombie still answers `kill(pid, 0)`. The CLI would
/// then wait out its whole grace period and reach SIGKILL for a process that
/// had already died.
///
/// The start time comes from the same source the registry's liveness check
/// reads, or the record would be indistinguishable from a recycled PID and the
/// reconcile pass would reset the group out from under the test.
fn orphan_process() -> (u32, GroupState) {
    let out = std::process::Command::new("sh")
        .args(["-c", "sleep 30 >/dev/null 2>&1 & printf %s \"$!\""])
        .output()
        .expect("spawn sleep");
    let pid: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("a pid on stdout");
    let state = GroupState::Running {
        pid,
        started_at: recorded_start_time(pid),
    };
    (pid, state)
}

/// Stops a leftover orphan, so a test never leaves a `sleep` behind.
fn reap(pid: u32) {
    conductor::dispatcher::signal_process(pid, Signal::SIGKILL);
}

/// Runs one event to completion, leaving the group as an operator would find it:
/// idle, with a started session and a real worktree.
///
/// Returns the dispatcher and sink so callers can submit more events into the
/// same running dispatcher rather than starting a second one.
async fn seeded(
    harness: &Harness,
    key: &GroupKey,
    identifier: &str,
    group: &str,
) -> (Arc<Dispatcher>, DispatchSink) {
    let (dispatcher, sink) = harness.start();
    sink.submit(linear_event(identifier, group, None));
    assert!(
        harness.wait_for_runs(1, PATIENCE).await,
        "the seeding run never finished"
    );
    assert!(
        harness
            .wait_for_state(key, PATIENCE, |s| *s == GroupState::Idle)
            .await,
        "the group never returned to idle after the seeding run"
    );
    (dispatcher, sink)
}

/// Runs `sessions attach` on a task, so the caller can watch the registry while
/// the interactive session is still going.
fn attach_in_background(
    harness: &Harness,
    group: &'static str,
    force: bool,
) -> tokio::task::JoinHandle<(i32, String)> {
    let env = harness.session_env();
    let registry = harness.registry.clone();
    tokio::spawn(async move {
        let mut out: Vec<u8> = Vec::new();
        let code = conductor::sessions::run(
            SessionsCommand::Attach {
                group: group.into(),
                force,
            },
            &env,
            &registry,
            &mut out,
        )
        .await
        .expect("attach");
        (code, String::from_utf8(out).expect("utf8 output"))
    })
}

/// Dispatched runs carry `--print`; an interactive attach never does. That is
/// what separates the two in the shared log.
fn dispatched_runs(harness: &Harness) -> usize {
    harness
        .starts()
        .iter()
        .filter(|r| r.argv.iter().any(|a| a == "--print"))
        .count()
}

// ── list ─────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn list_shows_busy_groups_first_with_one_row_each(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);

    let (idle, running, attached) = (named("alpha"), named("beta"), named("gamma"));
    for key in [&idle, &running, &attached] {
        harness
            .enqueue(key, &linear_event("NEX-1", "x", None))
            .await;
    }

    let (pid, run_state) = orphan_process();
    harness.set_state(&running, &run_state).await;
    harness
        .set_state(
            &attached,
            &GroupState::Attached {
                pid: std::process::id(),
                started_at: recorded_start_time(std::process::id()),
            },
        )
        .await;

    let (code, text) = harness
        .sessions(SessionsCommand::List { json: false })
        .await;
    assert_eq!(code, 0, "list should succeed: {text}");

    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("GROUP"), "header missing:\n{text}");
    assert_eq!(lines.len(), 4, "expected a header and three rows:\n{text}");

    let order: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l.split_whitespace().next().unwrap_or(""))
        .collect();
    assert_eq!(
        order,
        vec!["beta", "gamma", "alpha"],
        "expected running, then attached, then idle:\n{text}"
    );
    assert!(lines[1].contains("running"), "beta:\n{text}");
    assert!(lines[2].contains("attached"), "gamma:\n{text}");
    assert!(lines[3].contains("idle"), "alpha:\n{text}");

    reap(pid);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_json_carries_the_whole_group_not_the_summary(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    harness
        .enqueue(&named("alpha"), &linear_event("NEX-1", "alpha", None))
        .await;

    let (code, text) = harness.sessions(SessionsCommand::List { json: true }).await;
    assert_eq!(code, 0);

    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let groups = parsed.as_array().expect("an array of groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[0]["pending"].as_array().map(Vec::len),
        Some(1),
        "the queue should be included, which the table only counts"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_empty_registry_says_so_rather_than_printing_a_bare_header(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let (code, text) = harness
        .sessions(SessionsCommand::List { json: false })
        .await;
    assert_eq!(code, 0);
    assert!(text.contains("no session groups"), "{text}");
}

// ── unknown groups ───────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn an_unknown_group_exits_two_and_lists_what_does_exist(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    harness
        .enqueue(
            &named("real-group"),
            &linear_event("NEX-1", "real-group", None),
        )
        .await;

    let (code, text) = harness
        .sessions(SessionsCommand::Attach {
            group: "typo".into(),
            force: false,
        })
        .await;

    assert_eq!(code, 2, "unknown group should exit 2:\n{text}");
    assert!(text.contains("no group"), "{text}");
    // The point of exit 2 is that the operator can see what they meant.
    assert!(text.contains("real-group"), "{text}");
}

// ── attach ───────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn attach_holds_a_group_for_the_session_then_gives_it_back(pool: PgPool) {
    // Long enough that the attached state is observable while it runs.
    let harness = Harness::new(pool).with_fake(1_500, 0, None, false);
    let key = named("irr");
    let (_dispatcher, _sink) = seeded(&harness, &key, "NEX-1", "irr").await;
    let before = harness.starts().len();

    let attaching = attach_in_background(&harness, "irr", false);

    assert!(
        harness
            .wait_for_state(&key, PATIENCE, |s| matches!(s, GroupState::Attached { .. }))
            .await,
        "group was never marked attached"
    );

    let (code, text) = attaching.await.expect("join");
    assert_eq!(code, 0, "attach should succeed:\n{text}");
    assert_eq!(
        harness.state_of(&key).await,
        Some(GroupState::Idle),
        "the group must be idle once the session ends"
    );

    let starts = harness.starts();
    assert_eq!(starts.len(), before + 1, "expected exactly one attach");
    let argv = &starts.last().expect("an attach run").argv;
    assert!(
        argv.iter().any(|a| a == "--resume"),
        "a started session must be resumed, not restarted: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a == "--print"),
        "an interactive session must not be given --print: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a == "--dangerously-skip-permissions"),
        "a human is present to answer prompts: {argv:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn attach_waits_for_a_running_dispatch_then_takes_over(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(800, 0, None, false);
    let key = named("irr");

    let (_dispatcher, sink) = harness.start();
    sink.submit(linear_event("NEX-1", "irr", None));
    assert!(
        harness
            .wait_for_state(&key, PATIENCE, |s| matches!(s, GroupState::Running { .. }))
            .await,
        "the dispatched run never started"
    );

    let (code, text) = attach_in_background(&harness, "irr", false)
        .await
        .expect("join");

    assert_eq!(code, 0, "attach should succeed:\n{text}");
    assert!(
        text.contains("waiting for current run"),
        "the operator should be told why nothing is happening:\n{text}"
    );

    let group = harness.group(&key).await.expect("group");
    assert_eq!(
        group.last_run.map(|r| r.result),
        Some(RunResult::Exited { code: 0 }),
        "waiting must not disturb the run it waited for"
    );
    assert_eq!(group.state, GroupState::Idle);
}

#[sqlx::test(migrations = "./migrations")]
async fn attach_force_stops_the_running_session_and_records_a_kill(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(100, 0, None, false);
    let key = named("irr");
    let (_dispatcher, _sink) = seeded(&harness, &key, "NEX-1", "irr").await;

    // A live process with no dispatcher competing to record its death, which is
    // what keeps the recorded outcome deterministic.
    let (pid, state) = orphan_process();
    harness.set_state(&key, &state).await;

    let (code, text) = harness
        .sessions(SessionsCommand::Attach {
            group: "irr".into(),
            force: true,
        })
        .await;
    assert_eq!(code, 0, "forced attach should succeed:\n{text}");
    assert!(text.contains("stopping current run"), "{text}");

    let group = harness.group(&key).await.expect("group");
    assert_eq!(
        group.last_run.map(|r| r.result),
        Some(RunResult::Killed {
            by: "attach --force".into()
        }),
        "the kill should be attributed to the flag that caused it"
    );
    assert_eq!(group.state, GroupState::Idle, "and the group handed back");

    // Gone in fact, not merely recorded as gone.
    assert!(
        !process_is_alive(pid, started_at_of(&state)),
        "the stopped process should be gone"
    );
}

/// The start time recorded in a seeded state, for a liveness check afterwards.
fn started_at_of(state: &GroupState) -> chrono::DateTime<chrono::Utc> {
    match state {
        GroupState::Running { started_at, .. } | GroupState::Attached { started_at, .. } => {
            *started_at
        }
        GroupState::Idle => chrono::Utc::now(),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_group_cannot_be_attached_twice(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    harness
        .enqueue(&key, &linear_event("NEX-1", "irr", None))
        .await;
    harness
        .set_state(
            &key,
            &GroupState::Attached {
                pid: std::process::id(),
                started_at: recorded_start_time(std::process::id()),
            },
        )
        .await;

    let (code, text) = harness
        .sessions(SessionsCommand::Attach {
            group: "irr".into(),
            force: false,
        })
        .await;

    assert_eq!(code, 1, "a second attach should be refused:\n{text}");
    assert!(text.contains("already attached"), "{text}");
}

#[sqlx::test(migrations = "./migrations")]
async fn events_arriving_during_an_attach_wait_for_it_to_end(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(1_500, 0, None, false);
    let key = named("irr");
    // The same dispatcher stays up for the whole test, so the event below is
    // competing with a live scheduler rather than a dormant one.
    let (_dispatcher, sink) = seeded(&harness, &key, "NEX-1", "irr").await;
    let before = dispatched_runs(&harness);

    let attaching = attach_in_background(&harness, "irr", false);
    assert!(
        harness
            .wait_for_state(&key, PATIENCE, |s| matches!(s, GroupState::Attached { .. }))
            .await,
        "group was never marked attached"
    );

    sink.submit(linear_event("NEX-2", "irr", None));
    assert!(
        harness
            .wait_for_group(&key, PATIENCE, |g| !g.pending.is_empty())
            .await,
        "the event was never queued"
    );

    // Several of the harness's 100ms re-scans come round inside this.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        dispatched_runs(&harness),
        before,
        "an attached group must not be dispatched into"
    );

    attaching.await.expect("join");

    // Released, so the next re-scan picks the event up with no prompting.
    assert!(
        harness
            .wait_for(PATIENCE, |h| dispatched_runs(h) == before + 1)
            .await,
        "the queued event should run once the attach ends"
    );
}

// ── signals ──────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_hangup_ends_the_session_and_still_releases_the_group(pool: PgPool) {
    // Long enough that only a forwarded signal can end it inside PATIENCE.
    let harness = Harness::new(pool).with_fake(60_000, 0, None, false);
    let key = named("irr");
    // Deliberately not a dispatched run: this fake sleeps for a minute, so a
    // seeding run would blow PATIENCE before the test began. The group is
    // created directly instead, and `attach` builds the worktree itself —
    // which also covers attaching to a group that has never run.
    harness
        .enqueue(&key, &linear_event("NEX-1", "irr", None))
        .await;

    // A real process, because the behaviour under test is signal handling, and
    // an in-process attach would have the test runner catching the signal.
    let mut cli = harness.spawn_cli(&["sessions", "attach", "irr"]);
    assert!(
        harness
            .wait_for_state(&key, PATIENCE, |s| matches!(s, GroupState::Attached { .. }))
            .await,
        "the spawned CLI never attached"
    );

    // What a dropped `kubectl exec` sends.
    conductor::dispatcher::signal_process(cli.id(), Signal::SIGHUP);

    // The CLI installs a handler, so SIGHUP does not kill it outright. The only
    // way out is forwarding the signal, waiting for Claude to go, and reaching
    // the release. Had it swallowed the signal, the group would stay attached
    // until the fake's sixty seconds elapsed, long past PATIENCE.
    let released = harness
        .wait_for_state(&key, PATIENCE, |s| *s == GroupState::Idle)
        .await;
    if !released {
        let _ = cli.kill();
    }
    assert!(released, "the group was not released after SIGHUP");

    let status = cli.wait().expect("wait for cli");
    assert!(
        status.code().is_some(),
        "the CLI should exit on its own terms rather than be killed: {status:?}"
    );
}

// ── kill ─────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn kill_stops_the_process_and_leaves_the_queue_alone(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    harness
        .enqueue(&key, &linear_event("NEX-1", "irr", None))
        .await;
    harness
        .enqueue(&key, &linear_event("NEX-2", "irr", None))
        .await;

    let (pid, state) = orphan_process();
    harness.set_state(&key, &state).await;

    let (code, text) = harness
        .sessions(SessionsCommand::Kill {
            group: "irr".into(),
            drain: false,
        })
        .await;
    assert_eq!(code, 0, "{text}");

    assert!(
        !process_is_alive(pid, started_at_of(&state)),
        "the process should be gone"
    );

    let group = harness.group(&key).await.expect("group");
    assert_eq!(
        group.last_run.map(|r| r.result),
        Some(RunResult::Killed { by: "cli".into() })
    );
    assert_eq!(group.state, GroupState::Idle);
    assert_eq!(
        group.pending.len(),
        2,
        "killing a run must not discard accepted work"
    );
    assert!(
        text.contains("2 queued event"),
        "the operator should be told the work is still there:\n{text}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn kill_drain_discards_the_queue_and_names_what_it_dropped(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    harness
        .enqueue(&key, &linear_event("NEX-1", "irr", None))
        .await;
    harness
        .enqueue(&key, &linear_event("NEX-2", "irr", None))
        .await;

    let (pid, state) = orphan_process();
    harness.set_state(&key, &state).await;

    let (code, text) = harness
        .sessions(SessionsCommand::Kill {
            group: "irr".into(),
            drain: true,
        })
        .await;
    assert_eq!(code, 0, "{text}");

    let group = harness.group(&key).await.expect("group");
    assert!(
        group.pending.is_empty(),
        "--drain should empty the queue, found {}",
        group.pending.len()
    );
    // Dropped work is named, so it can be re-queued by hand if it mattered.
    assert!(text.contains("NEX-1"), "{text}");
    assert!(text.contains("NEX-2"), "{text}");

    reap(pid);
}

#[sqlx::test(migrations = "./migrations")]
async fn killing_an_idle_group_is_not_an_error(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    harness
        .enqueue(&key, &linear_event("NEX-1", "irr", None))
        .await;

    let (code, text) = harness
        .sessions(SessionsCommand::Kill {
            group: "irr".into(),
            drain: false,
        })
        .await;

    assert_eq!(code, 0, "an idle group is the desired end state:\n{text}");
    assert!(text.contains("not running"), "{text}");
}

// ── reset ────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn reset_makes_the_next_run_start_a_fresh_session_in_the_same_worktree(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    let (_dispatcher, sink) = seeded(&harness, &key, "NEX-1", "irr").await;

    let (old_session, resumed) = harness
        .starts()
        .last()
        .expect("a first run")
        .session()
        .expect("a session argument");
    assert!(!resumed, "the first run of a group starts a session");
    let worktree_before = harness.group(&key).await.expect("group").worktree;

    let (code, text) = harness
        .sessions(SessionsCommand::Reset {
            group: "irr".into(),
            force: false,
        })
        .await;
    assert_eq!(code, 0, "{text}");

    // Dispatch again and read what Claude was actually invoked with.
    sink.submit(linear_event("NEX-2", "irr", None));
    assert!(harness.wait_for_runs(2, PATIENCE).await, "second run");

    let (new_session, resumed) = harness
        .starts()
        .last()
        .expect("a second run")
        .session()
        .expect("a session argument");
    assert!(
        !resumed,
        "after a reset the next run must start a session, not resume the old one"
    );
    assert_ne!(new_session, old_session, "the session id should be new");
    assert_eq!(
        harness.group(&key).await.expect("group").worktree,
        worktree_before,
        "a reset keeps the worktree: only the conversation is discarded"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn reset_refuses_a_busy_group_unless_forced(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    harness
        .enqueue(&key, &linear_event("NEX-1", "irr", None))
        .await;

    let before = harness.group(&key).await.expect("group").session_id;
    let (pid, state) = orphan_process();
    harness.set_state(&key, &state).await;

    let (code, text) = harness
        .sessions(SessionsCommand::Reset {
            group: "irr".into(),
            force: false,
        })
        .await;
    assert_eq!(code, 1, "a running group should be refused:\n{text}");
    assert!(text.contains("--force"), "{text}");
    assert_eq!(
        harness.group(&key).await.expect("group").session_id,
        before,
        "a refused reset must change nothing"
    );

    let (code, text) = harness
        .sessions(SessionsCommand::Reset {
            group: "irr".into(),
            force: true,
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert_ne!(
        harness.group(&key).await.expect("group").session_id,
        before,
        "--force should reset after stopping the run"
    );
    assert!(
        !process_is_alive(pid, started_at_of(&state)),
        "--force should have stopped the process"
    );
}

// ── close ────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn close_removes_the_worktree_and_forgets_the_group(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    let (_dispatcher, _sink) = seeded(&harness, &key, "NEX-1", "irr").await;

    let worktree = harness.group(&key).await.expect("group").worktree;
    assert!(worktree.exists(), "the seeding run should have made one");

    let (code, text) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: false,
        })
        .await;
    assert_eq!(code, 0, "{text}");

    assert!(!worktree.exists(), "the worktree should be gone:\n{text}");
    assert!(
        harness.group(&key).await.is_none(),
        "the group should be gone"
    );
    // Branches outlive the group: the work on them may not be merged.
    assert!(text.contains("agent/irr"), "{text}");
    let branches = common::git(&harness.repo, &["branch", "--list", "agent/irr"]);
    assert!(
        branches.contains("agent/irr"),
        "close must not delete branches: {branches:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn close_refuses_a_dirty_worktree_and_leaves_it_untouched(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    let (_dispatcher, _sink) = seeded(&harness, &key, "NEX-1", "irr").await;

    let worktree = harness.group(&key).await.expect("group").worktree;
    let scratch = worktree.join("work-in-progress.txt");
    std::fs::write(&scratch, "uncommitted\n").expect("write");
    common::git(&worktree, &["add", "work-in-progress.txt"]);

    let (code, text) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: false,
        })
        .await;

    assert_eq!(code, 1, "a dirty worktree should be refused:\n{text}");
    assert!(worktree.exists(), "the worktree must survive a refusal");
    assert!(scratch.exists(), "and so must the uncommitted work");
    assert!(
        harness.group(&key).await.is_some(),
        "the group must survive too, or the registry would forget a live worktree"
    );
    assert!(
        text.contains("--force"),
        "the way out should be stated:\n{text}"
    );

    // And with --force it goes.
    let (code, text) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: true,
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(!worktree.exists(), "--force should discard the changes");
    assert!(harness.group(&key).await.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn close_refuses_a_group_with_queued_events_unless_forced(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    harness
        .enqueue(&key, &linear_event("NEX-1", "irr", None))
        .await;

    let (code, text) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: false,
        })
        .await;
    assert_eq!(code, 1, "queued work should block a close:\n{text}");
    assert!(text.contains("queued event"), "{text}");
    assert!(harness.group(&key).await.is_some());

    let (code, text) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: true,
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(harness.group(&key).await.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_closed_group_is_rebuilt_by_the_next_event_for_its_key(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    let (_dispatcher, sink) = seeded(&harness, &key, "NEX-1", "irr").await;
    let original = harness.group(&key).await.expect("group").session_id;

    let (code, _) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: true,
        })
        .await;
    assert_eq!(code, 0);
    assert!(harness.group(&key).await.is_none());

    sink.submit(linear_event("NEX-2", "irr", Some("fresh-delivery")));

    // Waiting on the registry, not on the fake's log: the child exits before
    // the dispatcher records the completion, so the issue history is written
    // strictly after the run the log reports as finished.
    assert!(
        harness
            .wait_for_group(&key, PATIENCE, |g| !g.issues.is_empty())
            .await,
        "the rebuilt group never recorded its issue"
    );

    let rebuilt = harness.group(&key).await.expect("group rebuilt");
    assert_ne!(
        rebuilt.session_id, original,
        "a rebuilt group is a new session, not the old one resurrected"
    );
    assert_eq!(
        rebuilt.issues,
        vec!["NEX-2".to_string()],
        "and it starts with no history from before the close"
    );
}
