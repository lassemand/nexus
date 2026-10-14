//! End-to-end tests of tmux-backed sessions: real tmux, a fake interactive
//! Claude (`fake-claude-tty`), real hooks through `conductor hook`, and the real
//! dispatcher and registry.
//!
//! The risky parts of the design are concurrency and terminal I/O: pasting twice,
//! pasting into a human's turn, a permit leaking when `Stop` never comes,
//! evicting a session somebody is watching, losing track of a session that died.
//! These tests check them against a real terminal rather than a fake runtime.
//!
//! Each test has its own tmux server (a random `-L` socket, killed on drop) and
//! hook listener, so none touches the default socket or a developer's tmux.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::tty::{expected_prompt, peak_overlap, TtyHarness};
use common::{linear_event, PATIENCE_TTY};
use conductor::linear::{ClosedIssue, ClosedKind};
use conductor::linear_api::{IssueSetSource, LinearApiError, SetIssue};
use conductor::registry::{GroupKey, GroupState, RunResult};
use conductor::sessions::SessionsCommand;
use conductor::tmux::SessionRuntime;
use conductor::{EventSink, InboundEvent};
use sqlx::PgPool;

fn named(group: &str) -> GroupKey {
    GroupKey::Named(group.into())
}

// ── dispatch ─────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn one_event_runs_through_a_real_session(pool: PgPool) {
    let t = TtyHarness::new(pool).await;
    let (_d, sink) = t.start().await;
    let key = named("solo");
    let event = linear_event("NEX-1", "solo", Some("d1"));
    sink.submit(event.clone());

    t.wait_done(&key, "NEX-1").await;

    // The session lives in the set's worktree.
    let group = t.group(&key).await;
    let cwd = t.tmux(&["display", "-p", "-t", "=solo:", "#{pane_current_path}"]);
    assert!(
        common::tty::same_path(std::path::Path::new(cwd.trim()), &group.worktree),
        "pane is in {cwd:?}, worktree {:?}",
        group.worktree
    );
    assert_eq!(t.sessions(), vec!["solo"]);

    // Delivered exactly as prompt_for builds it, and submitted once.
    assert_eq!(t.prompts(), vec![expected_prompt(&event)]);
    assert_eq!(t.of("paste").len(), 1);

    assert_eq!(
        group.last_run.expect("an outcome").result,
        RunResult::Completed
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_multi_line_prompt_is_one_submission(pool: PgPool) {
    let t = TtyHarness::new(pool).await;
    let (_d, sink) = t.start().await;
    let key = named("solo");
    t.submit(&sink, "NEX-1", "solo");
    t.wait_done(&key, "NEX-1").await;

    // Bracketed paste keeps the newlines from submitting early. The envelope
    // conductor builds is one line, so paste a multi-line one directly.
    let text = "first line\nsecond line\n\nfourth line";
    t.runtime.deliver("solo", text).await.expect("deliver");
    let ok =
        t.h.wait_for(PATIENCE_TTY, |_| t.of("start").len() == 2)
            .await;
    assert!(
        ok,
        "the second submission never happened: {:?}",
        t.entries()
    );

    assert_eq!(t.prompts()[1], text, "newlines survive, in one submission");
    assert_eq!(t.of("paste").len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn three_events_run_serially_in_one_process(pool: PgPool) {
    let t = TtyHarness::new(pool).await;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "300");
    let (_d, sink) = t.start().await;
    let key = named("serial");
    for n in 1..=3 {
        t.submit(&sink, &format!("NEX-{n}"), "serial");
    }
    t.wait_done(&key, "NEX-3").await;

    let starts = t.of("start");
    assert_eq!(starts.len(), 3);
    for (n, e) in starts.iter().enumerate() {
        assert!(e.prompt().contains(&format!("NEX-{}", n + 1)), "{:?}", e.0);
    }
    // One Claude process and one conversation served all three.
    assert_eq!(t.of("started").len(), 1, "the session must not restart");
    assert!(starts.iter().all(|e| e.pid() == starts[0].pid()));
    assert!(starts
        .iter()
        .all(|e| e.session_id() == starts[0].session_id()));
    assert_eq!(t.sessions(), vec!["serial"]);
    assert_eq!(peak_overlap(&t.entries()), 1, "turns never overlap");
}

#[sqlx::test(migrations = "./migrations")]
async fn at_most_the_cap_run_at_once_and_all_finish(pool: PgPool) {
    let mut t = TtyHarness::new(pool).await;
    t.h.config.max_sessions = 5;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "500");
    let (_d, sink) = t.start().await;

    // Sample the registry itself, not only the fake's log.
    let peak = Arc::new(AtomicUsize::new(0));
    let sampler = {
        let (registry, peak) = (t.h.registry.clone(), Arc::clone(&peak));
        tokio::spawn(async move {
            loop {
                if let Ok(groups) = registry.snapshot().await {
                    let active = groups
                        .iter()
                        .filter(|g| {
                            matches!(
                                g.state,
                                GroupState::Starting { .. }
                                    | GroupState::Delivering { .. }
                                    | GroupState::Busy { .. }
                            )
                        })
                        .count();
                    peak.fetch_max(active, Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };

    for n in 1..=8 {
        t.submit(&sink, &format!("NEX-{n}"), &format!("set-{n}"));
    }
    for n in 1..=8 {
        t.wait_done(&named(&format!("set-{n}")), &format!("NEX-{n}"))
            .await;
    }
    sampler.abort();

    let entries = t.entries();
    assert_eq!(t.of("start").len(), 8, "all eight finish");
    assert!(
        peak_overlap(&entries) <= 5,
        "{} turns overlapped",
        peak_overlap(&entries)
    );
    assert!(
        peak.load(Ordering::SeqCst) <= 5,
        "the registry showed {} active at once",
        peak.load(Ordering::SeqCst)
    );
    assert!(
        peak_overlap(&entries) >= 2,
        "the sets should have run in parallel"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn idle_ready_sessions_do_not_hold_permits(pool: PgPool) {
    let mut t = TtyHarness::new(pool).await;
    t.h.config.max_sessions = 5;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "100");
    let (_d, sink) = t.start().await;

    for n in 1..=5 {
        t.submit(&sink, &format!("NEX-{n}"), &format!("idle-{n}"));
    }
    for n in 1..=5 {
        t.wait_done(&named(&format!("idle-{n}")), &format!("NEX-{n}"))
            .await;
    }
    assert_eq!(t.sessions().len(), 5, "five idle sessions are resident");

    // With five permits and five resident sessions, a sixth set still runs.
    t.submit(&sink, "NEX-6", "sixth");
    t.wait_done(&named("sixth"), "NEX-6").await;
    assert_eq!(t.sessions().len(), 6);
}

// ── delivery that goes wrong ─────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn an_unsubmitted_paste_blocks_delivery_and_is_never_repeated(pool: PgPool) {
    let mut t = TtyHarness::new(pool).await;
    t.h.config.deliver_timeout = Duration::from_millis(800);
    t.set_knob("FAKE_CLAUDE_IGNORE_ENTER", "1");
    let (_d, sink) = t.start().await;
    let key = named("stuck");
    t.submit(&sink, "NEX-1", "stuck");

    assert!(
        t.h.wait_for_group(&key, PATIENCE_TTY, |g| g.delivery_blocked.is_some())
            .await,
        "the unconfirmed paste should block the set"
    );
    let group = t.group(&key).await;
    assert_eq!(group.pending.len(), 1, "the event stays queued");
    assert_eq!(t.of("paste").len(), 1);

    // A second event arrives: queued behind the block, not pasted.
    t.submit(&sink, "NEX-2", "stuck");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(t.of("paste").len(), 1, "no second paste, ever");
    assert_eq!(t.group(&key).await.pending.len(), 2);

    let (code, text) = t
        .cli(SessionsCommand::Release {
            group: "stuck".into(),
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("cleared the delivery block"), "{text}");
    assert!(t.group(&key).await.delivery_blocked.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_session_that_never_reports_in_fails_its_start_and_backs_off(pool: PgPool) {
    let mut t = TtyHarness::new(pool).await;
    t.h.config.start_timeout = Duration::from_millis(1500);
    t.h.config.start_backoff = Duration::from_secs(60);
    t.claude_never_starts();
    let (_d, sink) = t.start().await;
    let key = named("mute");
    t.submit(&sink, "NEX-1", "mute");

    assert!(
        t.h.wait_for_group(&key, PATIENCE_TTY, |g| g
            .last_run
            .as_ref()
            .is_some_and(|o| o.result == RunResult::StartFailed))
            .await,
        "the failed start should be recorded"
    );
    assert!(t.sessions().is_empty(), "the tmux session is killed");
    let group = t.group(&key).await;
    assert_eq!(group.state, GroupState::NoSession);
    assert_eq!(group.pending.len(), 1, "the event stays queued");

    // Backed off: left alone rather than retried in a loop.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(t.of("launched").len(), 1, "no second attempt");
    assert!(t.sessions().is_empty());
}

// ── the turn timeout ─────────────────────────────────────────────────────────

/// A turn that outlives the timeout gets Escape; whether `Stop` follows depends
/// on the real Claude (unverified), so both are covered.
async fn timeout_case(pool: PgPool, stop_on_interrupt: bool) {
    let mut t = TtyHarness::new(pool).await;
    t.h.config.run_timeout = Duration::from_millis(600);
    t.h.config.interrupt_grace = Duration::from_millis(1500);
    t.h.config.max_sessions = 1;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "60000");
    t.set_knob(
        "FAKE_CLAUDE_STOP_ON_INTERRUPT",
        if stop_on_interrupt { "1" } else { "0" },
    );
    let (_d, sink) = t.start().await;
    let key = named("slow");
    t.submit(&sink, "NEX-1", "slow");

    assert!(
        t.h.wait_for_group(&key, PATIENCE_TTY, |g| g
            .last_run
            .as_ref()
            .is_some_and(|o| o.result == RunResult::TimedOut))
            .await,
        "the timeout should be recorded"
    );
    assert!(
        t.h.wait_for(PATIENCE_TTY, |_| !t.of("interrupted").is_empty())
            .await,
        "Escape must reach the session"
    );

    if stop_on_interrupt {
        t.wait_state(&key, "ready", |s| matches!(s, GroupState::Ready { .. }))
            .await;
        assert_eq!(t.sessions(), vec!["slow"], "the session stays alive");
    } else {
        t.wait_state(&key, "no session", |s| *s == GroupState::NoSession)
            .await;
        assert!(t.sessions().is_empty(), "no Stop, so the session is killed");
    }
    // Recorded as timed out either way, and the event is not re-queued.
    let group = t.group(&key).await;
    assert_eq!(group.last_run.expect("outcome").result, RunResult::TimedOut);
    assert!(group.pending.is_empty());

    // The permit is back: with a cap of one, another set runs.
    t.set_knob("FAKE_CLAUDE_TURN_MS", "100");
    t.submit(&sink, "NEX-2", "after");
    t.wait_done(&named("after"), "NEX-2").await;
}

#[sqlx::test(migrations = "./migrations")]
async fn a_timed_out_turn_that_stops_on_escape_keeps_its_session(pool: PgPool) {
    timeout_case(pool, true).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn a_timed_out_turn_that_ignores_escape_loses_its_session(pool: PgPool) {
    timeout_case(pool, false).await;
}

// ── a session that dies ──────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_session_killed_mid_turn_is_lost_and_the_next_event_resumes(pool: PgPool) {
    let t = TtyHarness::new(pool).await;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "60000");
    let (_d, sink) = t.start().await;
    let key = named("fragile");
    t.submit(&sink, "NEX-1", "fragile");
    t.wait_state(&key, "busy", |s| matches!(s, GroupState::Busy { .. }))
        .await;

    let first = t.of("started")[0].clone();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(first.pid() as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill the fake claude");

    t.wait_state(&key, "no session", |s| *s == GroupState::NoSession)
        .await;
    let group = t.group(&key).await;
    assert_eq!(group.last_run.expect("outcome").result, RunResult::Lost);
    assert!(group.pending.is_empty(), "a lost turn is not re-queued");

    // The next event starts a new tmux session that resumes the conversation.
    t.set_knob("FAKE_CLAUDE_TURN_MS", "100");
    t.submit(&sink, "NEX-2", "fragile");
    t.wait_done(&key, "NEX-2").await;
    let started = t.of("started");
    assert_eq!(started.len(), 2);
    let argv = started[1].argv();
    let at = argv
        .iter()
        .position(|a| a == "--resume")
        .unwrap_or_else(|| panic!("the new session must --resume: {argv:?}"));
    assert_eq!(argv[at + 1], first.session_id(), "the same conversation");
}

// ── the live-session limit ───────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn the_idlest_unattached_session_is_evicted(pool: PgPool) {
    let mut t = TtyHarness::new(pool).await;
    t.h.config.max_live_sessions = 2;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "100");
    let (_d, sink) = t.start().await;

    t.submit(&sink, "NEX-1", "oldest");
    t.wait_done(&named("oldest"), "NEX-1").await;
    t.submit(&sink, "NEX-2", "middle");
    t.wait_done(&named("middle"), "NEX-2").await;
    t.submit(&sink, "NEX-3", "newest");
    t.wait_done(&named("newest"), "NEX-3").await;

    assert_eq!(t.sessions(), vec!["middle", "newest"]);
    assert_eq!(
        t.h.state_of(&named("oldest")).await,
        Some(GroupState::NoSession)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_session_with_a_client_attached_is_never_evicted(pool: PgPool) {
    let mut t = TtyHarness::new(pool).await;
    t.h.config.max_live_sessions = 2;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "100");
    let (_d, sink) = t.start().await;

    t.submit(&sink, "NEX-1", "watched");
    t.wait_done(&named("watched"), "NEX-1").await;
    t.submit(&sink, "NEX-2", "middle");
    t.wait_done(&named("middle"), "NEX-2").await;

    // Somebody is watching the idlest session.
    let _viewer = t.attach("watched", true);
    let mut attached = false;
    for _ in 0..400 {
        if t.clients("watched").await == 1 {
            attached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(attached, "the viewer never attached");

    t.submit(&sink, "NEX-3", "newest");
    t.wait_done(&named("newest"), "NEX-3").await;

    assert_eq!(
        t.sessions(),
        vec!["newest", "watched"],
        "the unwatched session made room"
    );
}

// ── restart ──────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn after_a_restart_every_set_starts_over_and_no_session_is_orphaned(pool: PgPool) {
    let t = TtyHarness::new(pool).await;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "100");
    let (_d, sink) = t.start().await;
    for name in ["alpha", "beta"] {
        t.submit(&sink, &format!("NEX-{name}"), name);
        t.wait_done(&named(name), &format!("NEX-{name}")).await;
    }
    assert_eq!(t.sessions(), vec!["alpha", "beta"]);

    // The pod restarts: the dispatcher is gone and so is tmux.
    t.stop_dispatcher();
    drop((_d, sink));
    t.kill_tmux_server();
    assert!(t.sessions().is_empty());

    let (_d2, sink2) = t.start().await;
    for name in ["alpha", "beta"] {
        t.wait_state(&named(name), "no session", |s| *s == GroupState::NoSession)
            .await;
    }

    // Work queued meanwhile is delivered after fresh starts.
    for name in ["alpha", "beta"] {
        t.submit(&sink2, &format!("NEX-again-{name}"), name);
    }
    for name in ["alpha", "beta"] {
        t.wait_done(&named(name), &format!("NEX-again-{name}"))
            .await;
    }
    assert_eq!(t.sessions(), vec!["alpha", "beta"]);
    assert_eq!(t.of("started").len(), 4, "two sessions, started twice");

    // A tmux session no group owns is cleaned up.
    t.tmux(&["new-session", "-d", "-s", "stray", "-x", "80", "-y", "24"]);
    assert!(t.sessions().contains(&"stray".to_string()));
    assert!(
        t.h.wait_for(PATIENCE_TTY, |_| !t
            .sessions()
            .contains(&"stray".to_string()))
            .await,
        "the orphan should be killed"
    );
}

// ── humans ───────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_person_typing_holds_back_the_next_task_until_their_turn_ends(pool: PgPool) {
    let t = TtyHarness::new(pool).await;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "1500");
    let (_d, sink) = t.start().await;
    let key = named("shared");
    t.submit(&sink, "NEX-1", "shared");
    t.wait_done(&key, "NEX-1").await;

    // Straight into the pane, bypassing conductor.
    t.type_into("shared", &["my own question", "Enter"]);
    t.wait_state(&key, "a human turn", |s| {
        matches!(
            s,
            GroupState::Busy {
                delivered_event: None,
                ..
            }
        )
    })
    .await;

    // A task arrives during their turn: queued, not pasted.
    t.submit(&sink, "NEX-2", "shared");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        t.of("paste").len(),
        1,
        "nothing is pasted into a human turn"
    );
    assert!(matches!(
        t.h.state_of(&key).await,
        Some(GroupState::Busy {
            delivered_event: None,
            ..
        })
    ));

    // Their turn ends; only then is the task delivered.
    t.wait_done(&key, "NEX-2").await;
    let prompts = t.prompts();
    assert!(prompts[0].contains("NEX-1"));
    assert_eq!(prompts[1], "my own question");
    assert!(prompts[2].contains("NEX-2"));
    assert_eq!(peak_overlap(&t.entries()), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn read_only_and_read_write_clients_share_a_busy_set(pool: PgPool) {
    let t = TtyHarness::new(pool).await;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "2500");
    let (_d, sink) = t.start().await;
    let key = named("crowd");
    t.submit(&sink, "NEX-1", "crowd");
    t.wait_state(&key, "busy", |s| matches!(s, GroupState::Busy { .. }))
        .await;

    // Both join while the agent is mid-turn.
    let mut watcher = t.attach("crowd", true);
    let mut typist = t.attach("crowd", false);
    let mut connected = false;
    for _ in 0..400 {
        if t.clients("crowd").await == 2 {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(connected, "both clients should be attached");

    // Dispatch carries on regardless of who is watching.
    t.submit(&sink, "NEX-2", "crowd");
    t.wait_done(&key, "NEX-2").await;

    // Now both type. The read-only client's keystrokes never reach the fake.
    watcher.send("from the watcher\r");
    tokio::time::sleep(Duration::from_millis(300)).await;
    typist.send("from the typist\r");
    assert!(
        t.h.wait_for(PATIENCE_TTY, |_| t
            .prompts()
            .iter()
            .any(|p| p == "from the typist"))
            .await,
        "the read-write client's keys must arrive: {:?}",
        t.prompts()
    );
    assert!(
        !t.prompts().iter().any(|p| p.contains("watcher")),
        "the read-only client must not be able to type: {:?}",
        t.prompts()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn attach_starts_a_missing_session_then_joins_it(pool: PgPool) {
    let t = TtyHarness::new(pool).await;
    let (_d, _sink) = t.start().await;
    let key = named("quiet");
    {
        let mut txn = t.h.registry.begin().await.expect("begin");
        txn.session_for_dispatch(&key).await.expect("create group");
        txn.commit().await.expect("commit");
    }
    assert!(t.sessions().is_empty());

    let _client = t.attach("quiet", false);
    let mut joined = false;
    for _ in 0..400 {
        if t.clients("quiet").await == 1 {
            joined = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(joined, "attach never connected");
    assert_eq!(t.sessions(), vec!["quiet"]);
    t.wait_state(&key, "ready", |s| matches!(s, GroupState::Ready { .. }))
        .await;
}

#[sqlx::test(migrations = "./migrations")]
async fn list_shows_clients_and_who_started_the_turn(pool: PgPool) {
    let t = TtyHarness::new(pool).await;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "2500");
    let (_d, sink) = t.start().await;
    let key = named("watchme");
    t.submit(&sink, "NEX-1", "watchme");
    t.wait_state(&key, "busy", |s| matches!(s, GroupState::Busy { .. }))
        .await;
    let _viewer = t.attach("watchme", true);
    for _ in 0..400 {
        if t.clients("watchme").await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let list = || SessionsCommand::List {
        json: false,
        closed: false,
        limit: 20,
    };
    let (_, text) = t.cli(list()).await;
    assert!(text.contains("CLIENTS"), "{text}");
    assert!(text.contains("busy (agent)"), "{text}");
    let row = text
        .lines()
        .find(|l| l.starts_with("watchme"))
        .expect("row");
    assert!(
        row.split_whitespace().any(|c| c == "1"),
        "one client: {row}"
    );

    // After the agent's turn, a person starts one.
    t.wait_done(&key, "NEX-1").await;
    t.type_into("watchme", &["a question from me", "Enter"]);
    t.wait_state(&key, "a human turn", |s| {
        matches!(
            s,
            GroupState::Busy {
                delivered_event: None,
                ..
            }
        )
    })
    .await;
    let (_, text) = t.cli(list()).await;
    assert!(text.contains("busy (you)"), "{text}");
}

// ── auto-close ───────────────────────────────────────────────────────────────

/// Says every issue in every set is completed.
struct AllCompleted;

#[async_trait::async_trait]
impl IssueSetSource for AllCompleted {
    async fn issues_in_set(&self, key: &GroupKey) -> Result<Vec<SetIssue>, LinearApiError> {
        let _ = key;
        Ok(vec![SetIssue {
            identifier: "NEX-1".into(),
            state_type: "completed".into(),
        }])
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_finished_set_is_not_closed_while_someone_is_attached(pool: PgPool) {
    let t = TtyHarness::new(pool).await;
    t.set_knob("FAKE_CLAUDE_TURN_MS", "100");
    let (_d, sink) = t.start_with(Some(Arc::new(AllCompleted))).await;
    let key = named("finished");
    t.submit(&sink, "NEX-1", "finished");
    t.wait_done(&key, "NEX-1").await;
    let worktree = t.group(&key).await.worktree;

    let mut viewer = t.attach("finished", true);
    for _ in 0..400 {
        if t.clients("finished").await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(t.clients("finished").await, 1);

    sink.submit(InboundEvent::LinearIssueClosed {
        issue: ClosedIssue {
            identifier: "NEX-1".into(),
            session_labels: vec!["finished".into()],
            kind: ClosedKind::Completed,
        },
        delivery_id: Some("closed-1".into()),
    });

    // Several scheduler passes go by; the viewer keeps the set open.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(t.h.group(&key).await.is_some(), "closed under a viewer");
    assert_eq!(t.sessions(), vec!["finished"]);
    assert!(worktree.exists());

    // Once they leave, the set closes: session gone, worktree removed.
    viewer.detach();
    let mut gone = false;
    for _ in 0..400 {
        if t.h.group(&key).await.is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "the set should close after the viewer leaves");
    assert!(t.sessions().is_empty(), "the tmux session is gone");
    assert!(!worktree.exists(), "the worktree is removed");
}
