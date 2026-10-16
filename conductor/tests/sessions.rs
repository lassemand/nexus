//! Tests for `conductor sessions`.
//!
//! Each set is an interactive Claude in a tmux session, so the CLI's job is to
//! join that terminal and to steer the session through the registry. The tests
//! therefore check the hand-over (what `attach` would exec, and that nothing is
//! locked or waited for), the control commands' effect on the registry and on
//! the fake tmux, and that the hooks keep tracking who started a turn.
//!
//! They run against the same fake tmux/Claude and temp-repo harness as the
//! dispatcher tests, so no real Claude or tmux runs.

mod common;

use std::time::Duration;

use common::{linear_event, FakeTerminal, Harness};
use conductor::registry::{GroupKey, GroupState, RunResult};
use conductor::sessions::SessionsCommand;
use conductor::tmux::SessionRuntime;
use conductor::EventSink;
use sqlx::PgPool;

/// How long any test may wait for work it expects to happen.
///
/// A ceiling for contention rather than an expected duration. A wrong
/// assertion still fails immediately.
const PATIENCE: Duration = Duration::from_secs(60);

fn named(group: &str) -> GroupKey {
    GroupKey::Named(group.into())
}

fn attach(group: &str) -> SessionsCommand {
    SessionsCommand::Attach {
        group: group.into(),
        read_only: false,
        force: false,
    }
}

/// Runs one event through a live dispatcher and leaves the set `Ready` with a
/// real session, as an operator would find it between tasks.
async fn ready_set(harness: &Harness, group: &str, identifier: &str) -> GroupKey {
    let key = named(group);
    let (_dispatcher, sink) = harness.start();
    sink.submit(linear_event(identifier, group, None));
    assert!(
        harness
            .wait_for_group(&key, PATIENCE, |g| {
                matches!(g.state, GroupState::Ready { .. }) && !g.issues.is_empty()
            })
            .await,
        "the set never became ready"
    );
    // The scheduler task keeps running without these handles; the ingest task
    // ends when the sink goes, which is fine because no more events are sent.
    drop((_dispatcher, sink));
    key
}

// ── list ─────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn list_shows_state_clients_and_who_started_the_turn(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);

    let (idle, agent_turn, human_turn, ready) = (
        named("alpha"),
        named("beta"),
        named("gamma"),
        named("delta"),
    );
    for key in [&idle, &agent_turn, &human_turn, &ready] {
        harness
            .enqueue(key, &linear_event("NEX-1", "x", None))
            .await;
    }
    let now = chrono::Utc::now();
    harness
        .set_state(
            &agent_turn,
            &GroupState::Busy {
                since: now,
                delivered_event: Some(1),
            },
        )
        .await;
    harness
        .set_state(
            &human_turn,
            &GroupState::Busy {
                since: now,
                delivered_event: None,
            },
        )
        .await;
    harness
        .set_state(
            &ready,
            &GroupState::Ready {
                pane_pid: 1,
                since: now,
            },
        )
        .await;
    // Sessions the fake knows about, so it can report clients.
    for g in [&agent_turn, &human_turn, &ready] {
        harness
            .fake
            .start(&g.slug(), &harness.repo, &["claude".into()], &[])
            .await
            .expect("fake session");
    }
    harness.fake.set_clients("delta", &["c1", "c2"]);

    let (code, text) = harness
        .sessions(SessionsCommand::List {
            json: false,
            closed: false,
            limit: 20,
        })
        .await;
    assert_eq!(code, 0, "list should succeed: {text}");

    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("GROUP"), "header missing:\n{text}");
    assert!(lines[0].contains("CLIENTS"), "{text}");
    assert_eq!(lines.len(), 5, "a header and four rows:\n{text}");

    let row = |name: &str| {
        lines
            .iter()
            .find(|l| l.split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("no row for {name}:\n{text}"))
            .to_string()
    };
    assert!(row("beta").contains("busy (agent)"), "{text}");
    assert!(row("gamma").contains("busy (you)"), "{text}");
    assert!(row("delta").contains("ready"), "{text}");
    assert!(row("alpha").contains("no session"), "{text}");
    let delta = row("delta");
    assert!(
        delta.split_whitespace().any(|c| c == "2"),
        "two clients on delta:\n{text}"
    );

    // Busy sets sort ahead of the rest.
    let order: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l.split_whitespace().next().unwrap_or(""))
        .collect();
    assert!(
        order[..2].contains(&"beta") && order[..2].contains(&"gamma"),
        "{order:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn list_marks_blocked_deliveries_and_closes(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("stuck");
    harness
        .enqueue(&key, &linear_event("NEX-1", "stuck", None))
        .await;
    sqlx::query(
        "UPDATE session_groups SET delivery_blocked = $2, close_blocked = $3 WHERE slug = $1",
    )
    .bind(key.slug())
    .bind("no UserPromptSubmit within 30s")
    .bind("worktree dirty")
    .execute(&harness.pool)
    .await
    .expect("mark blocked");

    let (_, text) = harness
        .sessions(SessionsCommand::List {
            json: false,
            closed: false,
            limit: 20,
        })
        .await;
    assert!(
        text.contains("delivery blocked: no UserPromptSubmit"),
        "{text}"
    );
    assert!(text.contains("close blocked: worktree dirty"), "{text}");
}

#[sqlx::test(migrations = "./migrations")]
async fn list_json_carries_the_whole_group_and_the_client_count(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    harness
        .enqueue(&named("alpha"), &linear_event("NEX-1", "alpha", None))
        .await;

    let (code, text) = harness
        .sessions(SessionsCommand::List {
            json: true,
            closed: false,
            limit: 20,
        })
        .await;
    assert_eq!(code, 0);

    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let groups = parsed.as_array().expect("an array of groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["pending"].as_array().map(Vec::len), Some(1));
    assert_eq!(groups[0]["clients"], 0);
    assert!(groups[0].get("delivery_blocked").is_some());
}

#[sqlx::test(migrations = "./migrations")]
async fn an_empty_registry_says_so_rather_than_printing_a_bare_header(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let (code, text) = harness
        .sessions(SessionsCommand::List {
            json: false,
            closed: false,
            limit: 20,
        })
        .await;
    assert_eq!(code, 0);
    assert!(text.contains("no session groups"), "{text}");
}

// ── attach ───────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn an_unknown_group_exits_two_and_lists_what_does_exist(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    harness
        .enqueue(
            &named("real-group"),
            &linear_event("NEX-1", "real-group", None),
        )
        .await;

    let terminal = FakeTerminal::interactive();
    let (code, text) = harness.sessions_on(attach("typo"), &terminal).await;

    assert_eq!(code, 2, "unknown group should exit 2:\n{text}");
    assert!(text.contains("no group"), "{text}");
    assert!(text.contains("real-group"), "{text}");
    assert!(terminal.execs.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn attach_joins_a_busy_set_without_waiting_or_locking(pool: PgPool) {
    // A long turn, so the set is plainly Busy while we attach.
    let harness = Harness::new(pool).with_fake(5_000, 0, None, false);
    let key = named("irr");
    let (_dispatcher, sink) = harness.start();
    sink.submit(linear_event("NEX-1", "irr", None));
    assert!(
        harness
            .wait_for_state(&key, PATIENCE, |s| matches!(s, GroupState::Busy { .. }))
            .await
    );

    let (first, second) = (FakeTerminal::interactive(), FakeTerminal::interactive());
    let started = std::time::Instant::now();
    let (a, b) = tokio::join!(
        harness.sessions_on(attach("irr"), &first),
        harness.sessions_on(attach("irr"), &second),
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "attach must not wait for the turn"
    );

    for (code, text) in [&a, &b] {
        assert_eq!(*code, 0, "{text}");
        assert!(
            text.contains(
                "attached to irr — detach with Ctrl-a d (read-write: anything you type goes to the agent)"
            ),
            "{text}"
        );
    }
    // Both joined the same session, read-write, with no -r.
    let expected = vec!["fake-tmux".to_string(), "attach".into(), "irr".into()];
    assert_eq!(*first.execs.lock().unwrap(), vec![expected.clone()]);
    assert_eq!(*second.execs.lock().unwrap(), vec![expected]);

    // Attaching claimed nothing: the turn carries on as the agent's.
    assert!(matches!(
        harness.state_of(&key).await,
        Some(GroupState::Busy {
            delivered_event: Some(_),
            ..
        })
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn read_only_attach_adds_the_read_only_flag(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;
    assert!(harness.fake.has(&key.slug()));

    let terminal = FakeTerminal::interactive();
    let (code, text) = harness
        .sessions_on(
            SessionsCommand::Attach {
                group: "irr".into(),
                read_only: true,
                force: false,
            },
            &terminal,
        )
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("read-only"), "{text}");
    assert_eq!(
        *terminal.execs.lock().unwrap(),
        vec![vec![
            "fake-tmux".to_string(),
            "attach".into(),
            "-r".into(),
            "irr".into()
        ]]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn attach_starts_a_missing_session_through_the_dispatchers_path(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    // A group with a session id and nothing running. The dispatcher is up (so
    // hooks are wired) but has nothing queued to start it.
    let key = named("irr");
    let (_dispatcher, _sink) = harness.start();
    {
        let mut txn = harness.registry.begin().await.expect("begin");
        txn.session_for_dispatch(&key).await.expect("create group");
        txn.commit().await.expect("commit");
    }

    let terminal = FakeTerminal::interactive();
    let (code, text) = harness.sessions_on(attach("irr"), &terminal).await;

    assert_eq!(code, 0, "{text}");
    assert!(text.contains("starting session for irr…"), "{text}");
    assert_eq!(*harness.fake.started.lock().unwrap(), vec!["irr"]);
    assert!(
        matches!(harness.state_of(&key).await, Some(GroupState::Ready { .. })),
        "the session must be Ready before the terminal is handed over"
    );
    assert_eq!(terminal.execs.lock().unwrap().len(), 1);
    // The worktree and model flag are the dispatcher's, not a separate recipe.
    let group = harness.group(&key).await.expect("group");
    assert!(group.worktree.join(".git").exists(), "a real worktree");
}

#[sqlx::test(migrations = "./migrations")]
async fn attach_that_cannot_start_a_session_fails_cleanly(pool: PgPool) {
    let mut harness = Harness::new(pool).with_fake(50, 0, None, false);
    harness.config.start_timeout = Duration::from_millis(300);
    harness
        .fake
        .send_session_start
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let key = named("irr");
    let (_dispatcher, _sink) = harness.start();
    {
        let mut txn = harness.registry.begin().await.expect("begin");
        txn.session_for_dispatch(&key).await.expect("create group");
        txn.commit().await.expect("commit");
    }

    let terminal = FakeTerminal::interactive();
    let (code, text) = harness.sessions_on(attach("irr"), &terminal).await;
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("could not start the session"), "{text}");
    assert!(terminal.execs.lock().unwrap().is_empty());
    assert!(
        !harness.fake.has("irr"),
        "the half-started session is killed"
    );
    let group = harness.group(&key).await.expect("group");
    assert_eq!(group.state, GroupState::NoSession);
    assert_eq!(
        group.last_run.map(|o| o.result),
        Some(RunResult::StartFailed)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn attach_to_a_registry_entry_tmux_no_longer_has_starts_a_new_session(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;
    // The tmux server restarted: the registry still says Ready.
    harness.fake.crash(&key.slug());

    let terminal = FakeTerminal::interactive();
    let (code, text) = harness.sessions_on(attach("irr"), &terminal).await;
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("starting session for irr…"), "{text}");
    assert!(harness.fake.has("irr"));
}

/// Deterministic version of the race with the dispatcher's tmux
/// reconciliation: the group is reset by someone else after attach has read it
/// as Ready but before attach's own reset.
#[sqlx::test(migrations = "./migrations")]
async fn attach_rereads_when_the_group_is_reset_under_it(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;

    let fake = harness.fake.clone();
    let registry = harness.registry.clone();
    let hook_key = key.clone();
    harness.fake.on_next_exists(Box::new(move || {
        Box::pin(async move {
            // What reconcile_tmux does when it finds the session missing.
            fake.crash(&hook_key.slug());
            let mut txn = registry.begin().await.expect("begin");
            txn.mark_session_gone(&hook_key, chrono::Duration::seconds(60))
                .await
                .expect("reset");
            txn.commit().await.expect("commit");
        })
    }));

    let terminal = FakeTerminal::interactive();
    let (code, text) = harness.sessions_on(attach("irr"), &terminal).await;
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("starting session for irr…"), "{text}");
    assert!(harness.fake.has("irr"));
}

#[sqlx::test(migrations = "./migrations")]
async fn attach_goes_ahead_while_delivering_or_starting(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;

    for state in [
        GroupState::Delivering {
            since: chrono::Utc::now(),
            event_id: 1,
        },
        GroupState::Starting {
            since: chrono::Utc::now(),
        },
    ] {
        harness.set_state(&key, &state).await;
        let terminal = FakeTerminal::interactive();
        let (code, text) = harness.sessions_on(attach("irr"), &terminal).await;
        assert_eq!(code, 0, "{state:?}: {text}");
        assert_eq!(terminal.execs.lock().unwrap().len(), 1, "{state:?}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn attach_needs_a_terminal_and_changes_nothing_without_one(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    harness
        .enqueue(&key, &linear_event("NEX-1", "irr", None))
        .await;

    let terminal = FakeTerminal::piped();
    let (code, text) = harness.sessions_on(attach("irr"), &terminal).await;
    assert_eq!(code, 1);
    assert!(
        text.contains(
            "attach needs an interactive terminal (use scripts/conductor from a terminal)"
        ),
        "{text}"
    );
    assert!(terminal.execs.lock().unwrap().is_empty());
    assert!(harness.fake.started.lock().unwrap().is_empty());
    assert_ne!(
        harness.state_of(&key).await,
        Some(GroupState::Starting {
            since: chrono::Utc::now()
        })
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn force_is_accepted_with_a_note_and_attaches_normally(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let _key = ready_set(&harness, "irr", "NEX-1").await;

    let terminal = FakeTerminal::interactive();
    let (code, text) = harness
        .sessions_on(
            SessionsCommand::Attach {
                group: "irr".into(),
                read_only: false,
                force: true,
            },
            &terminal,
        )
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("--force is no longer needed"), "{text}");
    assert_eq!(terminal.execs.lock().unwrap().len(), 1);
}

// ── a human typing while attached ────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_prompt_typed_while_attached_holds_back_the_next_task(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    let (_dispatcher, sink) = harness.start();
    sink.submit(linear_event("NEX-1", "irr", None));
    assert!(
        harness
            .wait_for_group(&key, PATIENCE, |g| {
                matches!(g.state, GroupState::Ready { .. }) && !g.issues.is_empty()
            })
            .await
    );

    // Someone joins and submits a prompt of their own.
    let terminal = FakeTerminal::interactive();
    let (code, _) = harness.sessions_on(attach("irr"), &terminal).await;
    assert_eq!(code, 0);
    harness.fake.hook("irr", "UserPromptSubmit").await;
    assert!(matches!(
        harness.state_of(&key).await,
        Some(GroupState::Busy {
            delivered_event: None,
            ..
        })
    ));

    // A new task arrives mid-way through their turn: queued, not pasted.
    let pasted_before = harness.fake.delivered.lock().unwrap().len();
    sink.submit(linear_event("NEX-2", "irr", None));
    assert!(
        harness
            .wait_for_group(&key, PATIENCE, |g| g.pending.len() == 1)
            .await
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        harness.fake.delivered.lock().unwrap().len(),
        pasted_before,
        "nothing may be pasted into a human's turn"
    );

    let (_, listing) = harness
        .sessions(SessionsCommand::List {
            json: false,
            closed: false,
            limit: 20,
        })
        .await;
    assert!(listing.contains("busy (you)"), "{listing}");

    // Their turn ends; only then does the queued task go in.
    harness.fake.hook("irr", "Stop").await;
    assert!(
        harness
            .wait_for(PATIENCE, |h| h.fake.delivered.lock().unwrap().len()
                == pasted_before + 1)
            .await,
        "the task should be delivered after the human's Stop"
    );
}

// ── interrupt ────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn interrupt_presses_escape_and_leaves_the_session_alive(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(5_000, 0, None, false);
    let key = named("irr");
    let (_dispatcher, sink) = harness.start();
    sink.submit(linear_event("NEX-1", "irr", None));
    assert!(
        harness
            .wait_for_state(&key, PATIENCE, |s| matches!(s, GroupState::Busy { .. }))
            .await
    );

    let (code, text) = harness
        .sessions(SessionsCommand::Interrupt {
            group: "irr".into(),
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert_eq!(*harness.fake.interrupted.lock().unwrap(), vec!["irr"]);
    assert!(harness.fake.has("irr"), "the session stays alive");
    assert!(harness.fake.killed.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn interrupt_without_a_session_says_so(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    harness
        .enqueue(&named("irr"), &linear_event("NEX-1", "irr", None))
        .await;
    let (code, text) = harness
        .sessions(SessionsCommand::Interrupt {
            group: "irr".into(),
        })
        .await;
    assert_eq!(code, 1);
    assert!(text.contains("no session"), "{text}");
    assert!(harness.fake.interrupted.lock().unwrap().is_empty());
}

// ── kill ─────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn kill_ends_the_session_records_the_turn_and_keeps_the_queue(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(5_000, 0, None, false);
    let key = named("irr");
    let (_dispatcher, sink) = harness.start();
    sink.submit(linear_event("NEX-1", "irr", None));
    assert!(
        harness
            .wait_for_state(&key, PATIENCE, |s| matches!(s, GroupState::Busy { .. }))
            .await
    );
    // Queued behind the running turn.
    sink.submit(linear_event("NEX-2", "irr", None));
    assert!(
        harness
            .wait_for_group(&key, PATIENCE, |g| g.pending.len() == 2)
            .await,
        "the running event stays queued until its turn ends"
    );

    let (code, text) = harness
        .sessions(SessionsCommand::Kill {
            group: "irr".into(),
            drain: false,
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(harness
        .fake
        .killed
        .lock()
        .unwrap()
        .contains(&"irr".to_string()));

    let group = harness.group(&key).await.expect("group");
    assert_eq!(
        group.last_run.expect("outcome").result,
        RunResult::Killed { by: "cli".into() }
    );
    // The killed turn's event is gone; the one behind it survives.
    assert_eq!(group.pending.len(), 1, "{text}");

    // The next event starts a fresh session that resumes the conversation.
    harness.fake.set_turn(20, None);
    assert!(
        harness
            .wait_for(PATIENCE, |h| h.fake.started.lock().unwrap().len() == 2)
            .await,
        "a fresh session should start for the queued event"
    );
    assert!(
        harness.wait_for(PATIENCE, |h| h.starts().len() == 2).await,
        "the queued event should be delivered into the new session"
    );
    let starts = harness.starts();
    let argv = &starts.last().expect("a turn").argv;
    assert!(argv.iter().any(|a| a == "--resume"), "{argv:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn kill_drain_discards_the_queue_and_names_what_it_dropped(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    harness
        .enqueue(&key, &linear_event("NEX-7", "irr", None))
        .await;
    harness
        .enqueue(&key, &linear_event("NEX-8", "irr", None))
        .await;

    let (code, text) = harness
        .sessions(SessionsCommand::Kill {
            group: "irr".into(),
            drain: true,
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("dropped 2 queued event(s)"), "{text}");
    assert!(text.contains("NEX-7") && text.contains("NEX-8"), "{text}");
    assert!(harness.group(&key).await.expect("group").pending.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn killing_a_set_with_no_session_is_not_an_error(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    harness
        .enqueue(&named("irr"), &linear_event("NEX-1", "irr", None))
        .await;
    let (code, text) = harness
        .sessions(SessionsCommand::Kill {
            group: "irr".into(),
            drain: false,
        })
        .await;
    assert_eq!(code, 0);
    assert!(text.contains("has no session"), "{text}");
    assert!(text.contains("1 queued event(s) still waiting"), "{text}");
}

// ── release ──────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn release_clears_a_blocked_delivery_and_a_live_session_goes_ready(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;
    harness
        .set_state(
            &key,
            &GroupState::Delivering {
                since: chrono::Utc::now(),
                event_id: 9,
            },
        )
        .await;
    sqlx::query("UPDATE session_groups SET delivery_blocked = 'unconfirmed paste' WHERE slug = $1")
        .bind(key.slug())
        .execute(&harness.pool)
        .await
        .expect("block");

    let (code, text) = harness
        .sessions(SessionsCommand::Release {
            group: "irr".into(),
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("cleared the delivery block"), "{text}");
    assert!(text.contains("delivering → ready"), "{text}");

    let group = harness.group(&key).await.expect("group");
    assert!(matches!(group.state, GroupState::Ready { .. }));
    assert!(group.delivery_blocked.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn release_resets_a_stuck_state_with_no_session_to_no_session(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = named("irr");
    harness
        .enqueue(&key, &linear_event("NEX-1", "irr", None))
        .await;
    harness
        .set_state(
            &key,
            &GroupState::Busy {
                since: chrono::Utc::now(),
                delivered_event: None,
            },
        )
        .await;

    let (code, text) = harness
        .sessions(SessionsCommand::Release {
            group: "irr".into(),
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("busy → no session"), "{text}");
    assert_eq!(harness.state_of(&key).await, Some(GroupState::NoSession));
}

#[sqlx::test(migrations = "./migrations")]
async fn release_with_nothing_to_do_says_so(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;
    let (code, text) = harness
        .sessions(SessionsCommand::Release {
            group: "irr".into(),
        })
        .await;
    assert_eq!(code, 0);
    assert!(text.contains("nothing to release"), "{text}");
    assert!(matches!(
        harness.state_of(&key).await,
        Some(GroupState::Ready { .. })
    ));
}

// ── reset ────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn reset_kills_the_session_before_minting_a_new_id(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;
    let before = harness.group(&key).await.expect("group");

    let (code, text) = harness
        .sessions(SessionsCommand::Reset {
            group: "irr".into(),
            force: false,
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(harness
        .fake
        .killed
        .lock()
        .unwrap()
        .contains(&"irr".to_string()));

    let after = harness.group(&key).await.expect("group");
    assert_eq!(after.state, GroupState::NoSession);
    assert_ne!(after.session_id, before.session_id, "a fresh conversation");
    assert!(!after.session_started);
    assert_eq!(after.worktree, before.worktree, "same worktree");
}

// ── close ────────────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn close_refuses_while_clients_are_attached(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;
    harness.fake.set_clients("irr", &["viewer"]);

    let (code, text) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: false,
        })
        .await;
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("1 client(s) attached"), "{text}");
    assert!(harness.group(&key).await.is_some(), "the group survives");
    assert!(harness.fake.has("irr"));
}

#[sqlx::test(migrations = "./migrations")]
async fn close_force_detaches_clients_kills_the_session_and_closes(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;
    harness.fake.set_clients("irr", &["viewer"]);

    let (code, text) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: true,
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert_eq!(*harness.fake.detached.lock().unwrap(), vec!["irr"]);
    assert!(!harness.fake.has("irr"), "the session is killed");
    assert!(harness.group(&key).await.is_none(), "the group is gone");
    assert!(text.contains("closed irr"), "{text}");
}

#[sqlx::test(migrations = "./migrations")]
async fn close_refuses_a_busy_set_unless_forced(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(5_000, 0, None, false);
    let key = named("irr");
    let (_dispatcher, sink) = harness.start();
    sink.submit(linear_event("NEX-1", "irr", None));
    assert!(
        harness
            .wait_for_state(&key, PATIENCE, |s| matches!(s, GroupState::Busy { .. }))
            .await
    );

    let (code, text) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: false,
        })
        .await;
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("busy"), "{text}");
    assert!(harness.group(&key).await.is_some());
}

#[sqlx::test(migrations = "./migrations")]
async fn close_removes_the_worktree_and_forgets_the_group(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;
    let worktree = harness.group(&key).await.expect("group").worktree;
    assert!(worktree.exists());

    let (code, text) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: false,
        })
        .await;
    assert_eq!(code, 0, "{text}");
    assert!(!worktree.exists(), "the worktree is removed");
    assert!(harness.group(&key).await.is_none());
    assert!(!harness.fake.has("irr"));
}

#[sqlx::test(migrations = "./migrations")]
async fn close_refuses_a_dirty_worktree_and_leaves_it_untouched(pool: PgPool) {
    let harness = Harness::new(pool).with_fake(50, 0, None, false);
    let key = ready_set(&harness, "irr", "NEX-1").await;
    let worktree = harness.group(&key).await.expect("group").worktree;
    std::fs::write(worktree.join("scratch.txt"), "uncommitted").expect("dirty the tree");

    let (code, text) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: false,
        })
        .await;
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("could not remove the worktree"), "{text}");
    assert!(worktree.join("scratch.txt").exists());
    assert!(harness.group(&key).await.is_some());
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
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("queued event"), "{text}");

    let (code, _) = harness
        .sessions(SessionsCommand::Close {
            group: "irr".into(),
            force: true,
        })
        .await;
    assert_eq!(code, 0);
    assert!(harness.group(&key).await.is_none());
}
