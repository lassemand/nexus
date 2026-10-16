//! Operator controls for the dispatcher's sessions.
//!
//! `conductor serve` runs unattended. These subcommands are how a human sees
//! what it is doing and steps in: join a set's live terminal, interrupt a turn,
//! kill or reset a session, clear a stuck delivery, or retire a finished group.
//!
//! They run inside the pod via `kubectl exec`, against the same registry the
//! server writes to. Coordination is entirely through Postgres: every mutation
//! is one locked transaction, and the dispatcher re-scans periodically, so a
//! change made here takes effect without any IPC channel between the two.
//!
//! Each set is an interactive Claude in a tmux session on the pod's tmux
//! server, so "attaching" is just joining that terminal. There is no exclusive
//! lock and no waiting: any number of clients can watch or type at once, and
//! the Claude hooks keep the registry's idea of busy/idle current regardless of
//! who typed. The registry lock is held only for read-modify-write, never across
//! a tmux call or a wait.

use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use clap::Subcommand;

use crate::closing::{close_group, CloseMode, CloseOutcome};
use crate::dispatcher::{launch_session, SessionEnv};
use crate::registry::{Group, GroupKey, GroupState, QueuedEvent, Registry, RunOutcome, RunResult};
use crate::tmux::SessionRuntime;
use crate::InboundEvent;

/// Exit code for a group that does not exist.
const EXIT_NO_SUCH_GROUP: i32 = 2;
/// Exit code for a refusal the operator can override with `--force`, or a failure.
const EXIT_REFUSED: i32 = 1;

/// A `Starting` group older than this with no tmux session is stale.
const STARTING_GRACE: i64 = 30;

/// What went wrong running a subcommand.
#[derive(Debug, thiserror::Error)]
pub enum SessionsError {
    #[error("registry: {0}")]
    Registry(#[from] sqlx::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Anything git or the filesystem refused, already phrased for an operator.
    #[error("{0}")]
    Failed(String),
}

/// The terminal `attach` hands over to tmux.
///
/// A trait so tests can observe the hand-over: the real one replaces the
/// process, which a test cannot survive.
pub trait Terminal: Send + Sync {
    /// Whether stdin and stdout are both a terminal.
    fn is_interactive(&self) -> bool;

    /// Replaces this process with `command` (program first). Returns only if
    /// that failed, as the exit code to use.
    fn exec(&self, command: &[String]) -> i32;
}

/// The process's real terminal.
pub struct StdTerminal;

impl Terminal for StdTerminal {
    fn is_interactive(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }

    fn exec(&self, command: &[String]) -> i32 {
        use std::os::unix::process::CommandExt;
        let Some((program, args)) = command.split_first() else {
            return EXIT_REFUSED;
        };
        // Only returns on failure; on success the terminal belongs to tmux.
        let error = std::process::Command::new(program).args(args).exec();
        tracing::error!(%error, %program, "cannot exec tmux");
        EXIT_REFUSED
    }
}

/// `conductor sessions <...>`
#[derive(Debug, clap::Args)]
pub struct SessionsArgs {
    #[command(flatten)]
    pub env: crate::dispatcher::SessionEnvArgs,

    #[command(subcommand)]
    pub command: SessionsCommand,
}

#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    /// Show every session group and what it is doing.
    List {
        /// Print the groups as JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Show recently closed groups instead of live ones.
        #[arg(long)]
        closed: bool,
        /// With --closed, how many rows to show.
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },

    /// Join a group's live terminal.
    ///
    /// Starts the group's session first if it has none. Then this process is
    /// replaced by `tmux attach-session`: you are in the same terminal the
    /// agent is working in, live, even mid-turn. Any number of people can attach
    /// at once. Read-write is the default, as before conductor; anything you
    /// type goes to the agent. A prompt you submit makes the group busy, and
    /// conductor waits for your turn to finish before giving it the next task.
    ///
    /// The pod's tmux prefix is Ctrl-a (not Ctrl-b, so it does not clash with a
    /// tmux running on your own machine): press Ctrl-a then d to detach. If you
    /// attach from inside a local tmux that also uses Ctrl-a, press it twice.
    Attach {
        /// Group to attach to, as shown by `sessions list`.
        group: String,
        /// Watch and scroll, but do not send keystrokes.
        #[arg(long, short = 'r')]
        read_only: bool,
        /// No longer needed: attaching never takes the session over.
        #[arg(long, hide = true)]
        force: bool,
    },

    /// Press Escape in a group's session, interrupting the current turn.
    ///
    /// The session stays alive.
    Interrupt {
        /// Group to interrupt.
        group: String,
    },

    /// Kill a group's tmux session.
    ///
    /// Claude exits and any turn in progress is recorded as killed. Queued
    /// events stay; the next one starts a fresh session that resumes the
    /// conversation.
    Kill {
        /// Group to stop.
        group: String,
        /// Also discard the group's queued events.
        #[arg(long)]
        drain: bool,
    },

    /// Clear a stuck delivery or a state that no longer matches reality.
    ///
    /// Clears the delivery block set when a pasted task was never confirmed.
    /// A group stuck delivering or busy goes to `no session` if its tmux session
    /// is gone, or `ready` if it is alive. Use it after attaching and checking
    /// that the input box is clean.
    Release {
        /// Group to release.
        group: String,
    },

    /// Start a fresh Claude conversation for a group, keeping its worktree.
    ///
    /// Kills the group's tmux session first, if it has one.
    Reset {
        /// Group to reset.
        group: String,
        /// No longer needed: a running session is always killed.
        #[arg(long, hide = true)]
        force: bool,
    },

    /// Retire a group: remove its worktree and forget it.
    ///
    /// Branches are left alone. A later event for the same key builds the group
    /// again from scratch. Refuses while anyone is attached or a turn is running
    /// unless --force, which detaches them and kills the session.
    Close {
        /// Group to close.
        group: String,
        /// Proceed despite attached clients, a running turn, queued events or a
        /// dirty worktree.
        #[arg(long)]
        force: bool,
    },
}

/// Runs one subcommand, returning the process exit code.
///
/// Output goes to `out` rather than `println!` so the formatting is testable
/// without capturing the process's stdout.
pub async fn run(
    command: SessionsCommand,
    env: &SessionEnv,
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    terminal: &dyn Terminal,
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    match command {
        SessionsCommand::List {
            json,
            closed,
            limit,
        } => {
            if closed {
                list_closed(registry, out, json, limit).await
            } else {
                list(registry, runtime, out, json).await
            }
        }
        SessionsCommand::Attach {
            group,
            read_only,
            force,
        } => {
            attach(
                &group, read_only, force, env, registry, runtime, terminal, out,
            )
            .await
        }
        SessionsCommand::Interrupt { group } => interrupt(&group, registry, runtime, out).await,
        SessionsCommand::Kill { group, drain } => kill(&group, drain, registry, runtime, out).await,
        SessionsCommand::Release { group } => release(&group, registry, runtime, out).await,
        SessionsCommand::Reset { group, force: _ } => reset(&group, registry, runtime, out).await,
        SessionsCommand::Close { group, force } => {
            close(&group, force, env, registry, runtime, out).await
        }
    }
}

// ── list ─────────────────────────────────────────────────────────────────────

/// How many tmux clients are attached to a group's session.
///
/// Only asked of groups that should have a session; a tmux call per group is
/// not worth making for the rest.
async fn client_count(runtime: &dyn SessionRuntime, g: &Group) -> usize {
    if g.state.is_tmux() && g.state != GroupState::NoSession {
        runtime.clients(&g.key.slug()).await.len()
    } else {
        0
    }
}

async fn list(
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    out: &mut (dyn Write + Send),
    json: bool,
) -> Result<i32, SessionsError> {
    // Unlocked read, so this works while the server is mid-write.
    let mut groups = registry.snapshot().await?;

    let mut clients = std::collections::HashMap::new();
    for g in &groups {
        clients.insert(g.key.slug(), client_count(runtime, g).await);
    }

    if json {
        let mut items = Vec::with_capacity(groups.len());
        for g in &groups {
            let mut v = serde_json::to_value(g)
                .map_err(|e| SessionsError::Failed(format!("serialising groups: {e}")))?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert("clients".into(), clients[&g.key.slug()].into());
            }
            items.push(v);
        }
        let body = serde_json::to_string_pretty(&items)
            .map_err(|e| SessionsError::Failed(format!("serialising groups: {e}")))?;
        writeln!(out, "{body}")?;
        return Ok(0);
    }

    if groups.is_empty() {
        writeln!(out, "no session groups")?;
        return Ok(0);
    }

    // Busy groups first, because they are what an operator is looking for.
    groups.sort_by(|a, b| {
        state_rank(&a.state)
            .cmp(&state_rank(&b.state))
            .then(b.last_active.cmp(&a.last_active))
    });

    let now = Utc::now();
    let header = [
        "GROUP",
        "STATE",
        "CLIENTS",
        "PENDING",
        "LAST ISSUE",
        "LAST ACTIVE",
        "LAST RUN",
        "MODEL",
        "SESSION",
        "WORKTREE",
    ];
    let mut rows: Vec<Vec<String>> = vec![header.iter().map(|h| h.to_string()).collect()];
    for g in &groups {
        rows.push(vec![
            g.key.slug(),
            describe_group_state(g, now),
            clients[&g.key.slug()].to_string(),
            g.pending.len().to_string(),
            g.issues.last().cloned().unwrap_or_else(|| "-".into()),
            format!("{} ago", human_duration(now - g.last_active)),
            describe_outcome(g.last_run.as_ref()),
            g.last_run
                .as_ref()
                .and_then(|o| o.model.clone())
                .unwrap_or_else(|| "-".into()),
            // Enough to correlate with a log line without filling the row.
            g.session_id.to_string()[..8].to_string(),
            display_path(&g.worktree),
        ]);
    }

    write_table(out, &rows)?;
    Ok(0)
}

/// Sort key putting active groups first.
fn state_rank(state: &GroupState) -> u8 {
    match state {
        GroupState::Running { .. } | GroupState::Busy { .. } | GroupState::Delivering { .. } => 0,
        GroupState::Attached { .. } | GroupState::Starting { .. } | GroupState::Ready { .. } => 1,
        GroupState::Idle | GroupState::NoSession => 2,
    }
}

/// The STATE column: the session state, then any marker an operator must see.
fn describe_group_state(g: &Group, now: DateTime<Utc>) -> String {
    let mut text = describe_state(&g.state, now);
    if let Some(reason) = &g.delivery_blocked {
        let reason: String = reason.chars().take(60).collect();
        text.push_str(&format!(", delivery blocked: {reason}"));
    }
    if let Some(reason) = &g.close_blocked {
        let reason: String = reason.chars().take(60).collect();
        text.push_str(&format!(", close blocked: {reason}"));
    } else if g.close_requested_at.is_some() {
        text.push_str(", closing");
    }
    text
}

/// `sessions list --closed`: the audit trail, newest first.
async fn list_closed(
    registry: &Registry,
    out: &mut (dyn Write + Send),
    json: bool,
    limit: i64,
) -> Result<i32, SessionsError> {
    let closed = registry.recent_closed(limit.max(0)).await?;
    if json {
        let body = serde_json::to_string_pretty(&closed)
            .map_err(|e| SessionsError::Failed(format!("serialising groups: {e}")))?;
        writeln!(out, "{body}")?;
        return Ok(0);
    }
    if closed.is_empty() {
        writeln!(out, "no closed groups")?;
        return Ok(0);
    }
    let now = Utc::now();
    let mut rows: Vec<Vec<String>> = vec![["GROUP", "CLOSED", "MODE", "ISSUES", "SESSION"]
        .iter()
        .map(|h| h.to_string())
        .collect()];
    for c in &closed {
        rows.push(vec![
            c.slug.clone(),
            format!("{} ago", human_duration(now - c.closed_at)),
            c.mode.clone(),
            if c.issues.is_empty() {
                "-".into()
            } else {
                c.issues.join(",")
            },
            c.session_id.to_string()[..8].to_string(),
        ]);
    }
    write_table(out, &rows)?;
    Ok(0)
}

fn describe_state(state: &GroupState, now: DateTime<Utc>) -> String {
    match state {
        GroupState::Idle | GroupState::NoSession => "no session".into(),
        GroupState::Starting { .. } => "starting".into(),
        GroupState::Ready { .. } => "ready".into(),
        GroupState::Delivering { .. } => "delivering".into(),
        // Who started the turn matters: "you" means a human typed it.
        GroupState::Busy {
            since,
            delivered_event,
        } => format!(
            "busy ({}) {}",
            if delivered_event.is_some() {
                "agent"
            } else {
                "you"
            },
            human_duration(now - *since)
        ),
        // Written by versions that ran `claude --print`; recovered at startup.
        GroupState::Running { .. } | GroupState::Attached { .. } => "legacy".into(),
    }
}

fn describe_outcome(outcome: Option<&RunOutcome>) -> String {
    let Some(o) = outcome else {
        return "-".into();
    };
    match &o.result {
        RunResult::Completed => "completed".into(),
        RunResult::StartFailed => "start failed".into(),
        RunResult::Exited { code } => format!("exited {code}"),
        RunResult::Signaled { signal } => format!("signaled {signal}"),
        RunResult::TimedOut => "timed out".into(),
        RunResult::Killed { by } => format!("killed ({by})"),
        RunResult::Interrupted => "interrupted".into(),
        RunResult::Lost => "lost".into(),
    }
}

fn display_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        "-".into()
    } else {
        path.display().to_string()
    }
}

/// Coarse, human-sized duration: the operator wants a glance, not precision.
fn human_duration(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Writes left-aligned columns, each sized to its widest cell.
fn write_table(out: &mut (dyn Write + Send), rows: &[Vec<String>]) -> std::io::Result<()> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    for row in rows {
        let last = row.len().saturating_sub(1);
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i == last {
                // No trailing padding, so copied output has no stray spaces.
                line.push_str(cell);
            } else {
                line.push_str(&format!("{:<width$}  ", cell, width = widths[i]));
            }
        }
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(())
}

// ── attach ───────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn attach(
    slug: &str,
    read_only: bool,
    force: bool,
    env: &SessionEnv,
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    terminal: &dyn Terminal,
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    let Some(group) = lookup(registry, slug, out).await? else {
        return Ok(EXIT_NO_SUCH_GROUP);
    };
    let key = group.key.clone();

    // Before anything is started: tmux would otherwise fail with a cryptic
    // message, after we had already spun up a session for nobody.
    if !terminal.is_interactive() {
        writeln!(
            out,
            "attach needs an interactive terminal (use scripts/conductor from a terminal)"
        )?;
        return Ok(EXIT_REFUSED);
    }
    if force {
        writeln!(
            out,
            "--force is no longer needed: attaching never takes the session over"
        )?;
    }

    // A few rounds, because the dispatcher may change the group between our
    // read and our write; each round re-reads.
    for _ in 0..5 {
        let Some(current) = registry.group_by_slug(slug).await? else {
            writeln!(out, "group {slug} disappeared")?;
            return Ok(EXIT_NO_SUCH_GROUP);
        };
        let mut state = current.state;

        if matches!(
            state,
            GroupState::Running { .. } | GroupState::Attached { .. }
        ) {
            writeln!(
                out,
                "{slug} is in a state left by an older conductor; start `conductor serve` once to recover it"
            )?;
            return Ok(EXIT_REFUSED);
        }

        // The registry can name a session tmux no longer has (a restart, a
        // crash). Reset it rather than attaching to nothing.
        if state.is_tmux() && state != GroupState::NoSession && !runtime.exists(slug).await {
            let mut txn = registry.begin().await?;
            let reset = txn
                .mark_session_gone(&key, chrono::Duration::seconds(STARTING_GRACE))
                .await?;
            txn.commit().await?;
            if reset {
                state = GroupState::NoSession;
            } else {
                // Someone else (the dispatcher's reconciliation) changed the
                // group first; our local state is stale, so look again.
                continue;
            }
        }

        match state {
            GroupState::Idle | GroupState::NoSession => {
                writeln!(out, "starting session for {slug}…")?;
                out.flush()?;
                let mut txn = registry.begin().await?;
                let claimed = txn.begin_start(&key).await?;
                txn.commit().await?;
                if !claimed {
                    // The dispatcher got there first; look again.
                    continue;
                }
                if let Err(reason) = start_claimed(registry, runtime, env, &key).await {
                    writeln!(out, "could not start the session: {reason}")?;
                    return Ok(EXIT_REFUSED);
                }
            }
            // Somebody else is starting it; join once tmux has the session.
            GroupState::Starting { .. }
                if !wait_for_session(runtime, slug, env.start_timeout).await =>
            {
                writeln!(out, "{slug} is starting but its session never appeared")?;
                return Ok(EXIT_REFUSED);
            }
            // Ready, Delivering, Busy, or Starting with a session: attach right
            // away, whatever it is doing.
            _ => {}
        }

        let command = runtime.attach_command(slug, read_only);
        if read_only {
            writeln!(
                out,
                "attached to {slug} read-only — detach with Ctrl-a d (you can watch and scroll, not type)"
            )?;
        } else {
            writeln!(
                out,
                "attached to {slug} — detach with Ctrl-a d (read-write: anything you type goes to the agent)"
            )?;
        }
        // Nothing buffered may be lost: exec replaces the process.
        out.flush()?;
        return Ok(terminal.exec(&command));
    }

    writeln!(out, "{slug} kept changing state; try again")?;
    Ok(EXIT_REFUSED)
}

/// Finishes starting a group this process has moved to `Starting`, the way the
/// dispatcher does, undoing it if the session never comes up.
async fn start_claimed(
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    env: &SessionEnv,
    key: &GroupKey,
) -> Result<(), String> {
    let result = launch_session(registry, runtime, &env.start_spec(), key, None, None).await;
    if let Err(reason) = &result {
        tracing::error!(group = %key.slug(), %reason, "session failed to start");
        let _ = runtime.kill(&key.slug()).await;
        let recorded = async {
            let mut txn = registry.begin().await?;
            txn.fail_start(key).await?;
            txn.commit().await
        }
        .await;
        if let Err(e) = recorded {
            tracing::error!(error = %e, "cannot record the failed start");
        }
    }
    result
}

/// Polls until tmux has the session, or `timeout` passes.
async fn wait_for_session(
    runtime: &dyn SessionRuntime,
    slug: &str,
    timeout: std::time::Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if runtime.exists(slug).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

// ── interrupt ────────────────────────────────────────────────────────────────

async fn interrupt(
    slug: &str,
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    let Some(group) = lookup(registry, slug, out).await? else {
        return Ok(EXIT_NO_SUCH_GROUP);
    };
    if !runtime.exists(&group.key.slug()).await {
        writeln!(out, "{slug} has no session to interrupt")?;
        return Ok(EXIT_REFUSED);
    }
    runtime
        .interrupt(slug)
        .await
        .map_err(|e| SessionsError::Failed(format!("cannot interrupt {slug}: {e}")))?;
    writeln!(
        out,
        "sent Escape to {slug}; the session stays alive (state: {})",
        describe_state(&group.state, Utc::now())
    )?;
    Ok(0)
}

// ── kill ─────────────────────────────────────────────────────────────────────

/// Ends a group's tmux session as the operator asked: the turn in progress is
/// recorded as `Killed { by: "cli" }`, the state becomes `NoSession`, and tmux
/// is told to kill the session. Returns whether there was anything to end.
async fn end_session_by_cli(
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    group: &Group,
) -> Result<bool, SessionsError> {
    let slug = group.key.slug();
    let mut txn = registry.begin().await?;
    let changed = txn
        .end_session(
            &group.key,
            RunResult::Killed { by: "cli".into() },
            None,
            chrono::Duration::zero(),
        )
        .await?;
    txn.commit().await?;

    // Asked even when the registry thought there was no session: a stray tmux
    // session is exactly what an operator running `kill` wants gone.
    let existed = runtime.exists(&slug).await;
    runtime
        .kill(&slug)
        .await
        .map_err(|e| SessionsError::Failed(format!("cannot kill the session for {slug}: {e}")))?;
    Ok(changed || existed)
}

async fn kill(
    slug: &str,
    drain: bool,
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    let Some(group) = lookup(registry, slug, out).await? else {
        return Ok(EXIT_NO_SUCH_GROUP);
    };

    let turn = matches!(
        group.state,
        GroupState::Busy { .. } | GroupState::Delivering { .. }
    );
    if end_session_by_cli(registry, runtime, &group).await? {
        if turn {
            writeln!(
                out,
                "killed the session for {slug}; its turn is recorded as killed"
            )?;
        } else {
            writeln!(out, "killed the session for {slug}")?;
        }
    } else {
        writeln!(out, "{slug} has no session")?;
    }

    if drain {
        let mut txn = registry.begin().await?;
        let dropped = txn.drain_pending(&group.key).await?;
        txn.commit().await?;
        if dropped.is_empty() {
            writeln!(out, "no queued events to drop")?;
        } else {
            writeln!(out, "dropped {} queued event(s):", dropped.len())?;
            for event in &dropped {
                writeln!(out, "  {}", summarise(event))?;
            }
        }
    } else {
        // Said explicitly: queued work surviving a kill is the surprising part.
        let queued = registry
            .group_by_slug(slug)
            .await?
            .map(|g| g.pending.len())
            .unwrap_or(0);
        writeln!(
            out,
            "{queued} queued event(s) still waiting; the next one starts a fresh session that resumes the conversation"
        )?;
    }

    Ok(0)
}

// ── release ──────────────────────────────────────────────────────────────────

async fn release(
    slug: &str,
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    let Some(group) = lookup(registry, slug, out).await? else {
        return Ok(EXIT_NO_SUCH_GROUP);
    };
    let alive = runtime.exists(slug).await;
    let pane_pid = if alive {
        runtime.pane_pid(slug).await.unwrap_or(0)
    } else {
        0
    };

    let mut txn = registry.begin().await?;
    let changes = txn.release_session(&group.key, alive, pane_pid).await?;
    txn.commit().await?;

    if changes.is_empty() {
        writeln!(out, "nothing to release for {slug}")?;
    } else {
        for change in &changes {
            writeln!(out, "{slug}: {change}")?;
        }
    }
    Ok(0)
}

/// One line describing a queued event, for an operator about to discard it.
fn summarise(queued: &QueuedEvent) -> String {
    let received = queued.received_at.to_rfc3339();
    match &queued.event {
        InboundEvent::LinearIssueTodo { issue, .. } => {
            format!("{received}  linear {} — {}", issue.identifier, issue.title)
        }
        InboundEvent::LinearIssueClosed { issue, .. } => {
            format!(
                "{received}  linear {} — closed ({:?})",
                issue.identifier, issue.kind
            )
        }
        InboundEvent::GithubPrComment { comment, kind, .. } => format!(
            "{received}  github {} {}#{} comment {}",
            kind.as_str(),
            comment.repo,
            comment.pr_number,
            comment.comment_id
        ),
    }
}

// ── reset ────────────────────────────────────────────────────────────────────

async fn reset(
    slug: &str,
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    let Some(group) = lookup(registry, slug, out).await? else {
        return Ok(EXIT_NO_SUCH_GROUP);
    };

    // The old conversation's session must not outlive its id.
    if end_session_by_cli(registry, runtime, &group).await? {
        writeln!(out, "killed the running session for {slug}")?;
    }

    let mut txn = registry.begin().await?;
    let session_id = txn.reset_session(&group.key).await?;
    txn.commit().await?;

    writeln!(
        out,
        "{slug} will start a fresh session ({}) on its next event, in the same worktree",
        &session_id.to_string()[..8]
    )?;
    Ok(0)
}

// ── close ────────────────────────────────────────────────────────────────────

async fn close(
    slug: &str,
    force: bool,
    env: &SessionEnv,
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    let Some(group) = lookup(registry, slug, out).await? else {
        return Ok(EXIT_NO_SUCH_GROUP);
    };

    let live = group.state.is_tmux() && group.state != GroupState::NoSession;
    let clients = if live {
        runtime.clients(slug).await.len()
    } else {
        0
    };

    if !force {
        if live && !matches!(group.state, GroupState::Ready { .. }) {
            writeln!(
                out,
                "{slug} is {}; use --force to kill it and close anyway",
                describe_state(&group.state, Utc::now())
            )?;
            return Ok(EXIT_REFUSED);
        }
        if clients > 0 {
            writeln!(
                out,
                "{slug} has {clients} client(s) attached; use --force to detach them and close"
            )?;
            return Ok(EXIT_REFUSED);
        }
        if !group.pending.is_empty() {
            writeln!(
                out,
                "{slug} has {} queued event(s); use --force to discard them and close",
                group.pending.len()
            )?;
            return Ok(EXIT_REFUSED);
        }
    } else if clients > 0 {
        writeln!(out, "detaching {clients} client(s) from {slug}…")?;
        runtime
            .detach_clients(slug)
            .await
            .map_err(|e| SessionsError::Failed(format!("cannot detach clients: {e}")))?;
    }

    let mode = CloseMode::Manual { force };
    let (branches, _issues) =
        match close_group(registry, &env.repo_root, &group.key, mode, Some(runtime)).await? {
            CloseOutcome::Closed { branches, issues } => (branches, issues),
            CloseOutcome::Gone => {
                writeln!(out, "{slug} was already gone")?;
                return Ok(0);
            }
            CloseOutcome::WorktreeDirty(e) => {
                let path = group.worktree.to_string_lossy();
                writeln!(
                    out,
                    "could not remove the worktree at {path}, so {slug} was left alone: {e}"
                )?;
                if !force {
                    writeln!(
                        out,
                        "if the worktree has uncommitted changes, --force discards them"
                    )?;
                }
                return Ok(EXIT_REFUSED);
            }
            // Only reachable without --force, when state changed after the lookup.
            CloseOutcome::Busy => {
                writeln!(
                    out,
                    "{slug} became busy or was attached; use --force to kill it and close anyway"
                )?;
                return Ok(EXIT_REFUSED);
            }
            CloseOutcome::HasPending => {
                writeln!(
                    out,
                    "{slug} has queued event(s); use --force to discard them and close"
                )?;
                return Ok(EXIT_REFUSED);
            }
        };

    if !group.worktree.as_os_str().is_empty() {
        writeln!(out, "removed worktree {}", group.worktree.display())?;
    }
    writeln!(out, "closed {slug}")?;
    if branches.is_empty() {
        writeln!(out, "no branches were recorded for it")?;
    } else {
        // Left behind deliberately: the work on them may not be merged yet.
        writeln!(out, "these branches are left in place:")?;
        for branch in &branches {
            writeln!(out, "  {branch}")?;
        }
    }
    Ok(0)
}

// ── shared ───────────────────────────────────────────────────────────────────

/// Finds a group by slug, reporting the known keys when there is no match.
async fn lookup(
    registry: &Registry,
    slug: &str,
    out: &mut (dyn Write + Send),
) -> Result<Option<Group>, SessionsError> {
    if let Some(group) = registry.group_by_slug(slug).await? {
        return Ok(Some(group));
    }

    writeln!(out, "no group {slug:?}")?;
    let known = registry.snapshot().await?;
    if known.is_empty() {
        writeln!(out, "there are no session groups")?;
    } else {
        writeln!(out, "known groups:")?;
        for group in &known {
            writeln!(out, "  {}", group.key.slug())?;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn outcome(result: RunResult) -> RunOutcome {
        RunOutcome {
            result,
            finished_at: Utc::now(),
            duration: Duration::from_secs(1),
            model: None,
        }
    }

    #[test]
    fn durations_read_as_the_largest_useful_unit() {
        assert_eq!(human_duration(chrono::Duration::seconds(0)), "0s");
        assert_eq!(human_duration(chrono::Duration::seconds(59)), "59s");
        assert_eq!(human_duration(chrono::Duration::seconds(60)), "1m");
        assert_eq!(human_duration(chrono::Duration::minutes(59)), "59m");
        assert_eq!(human_duration(chrono::Duration::hours(3)), "3h");
        assert_eq!(human_duration(chrono::Duration::hours(23)), "23h");
        assert_eq!(human_duration(chrono::Duration::days(2)), "2d");
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_underflow() {
        // Two processes with slightly different clocks would otherwise produce
        // a negative duration and a panic in release.
        assert_eq!(human_duration(chrono::Duration::seconds(-5)), "0s");
    }

    #[test]
    fn every_outcome_has_a_readable_form() {
        assert_eq!(describe_outcome(None), "-");
        assert_eq!(
            describe_outcome(Some(&outcome(RunResult::Exited { code: 0 }))),
            "exited 0"
        );
        assert_eq!(
            describe_outcome(Some(&outcome(RunResult::Signaled { signal: 15 }))),
            "signaled 15"
        );
        assert_eq!(
            describe_outcome(Some(&outcome(RunResult::TimedOut))),
            "timed out"
        );
        assert_eq!(
            describe_outcome(Some(&outcome(RunResult::Killed { by: "cli".into() }))),
            "killed (cli)"
        );
        assert_eq!(
            describe_outcome(Some(&outcome(RunResult::Interrupted))),
            "interrupted"
        );
        assert_eq!(describe_outcome(Some(&outcome(RunResult::Lost))), "lost");
    }

    #[test]
    fn busy_groups_sort_above_idle_ones() {
        let now = Utc::now();
        let mut ranks = [
            state_rank(&GroupState::Idle),
            state_rank(&GroupState::Attached {
                pid: 1,
                started_at: now,
            }),
            state_rank(&GroupState::Running {
                pid: 1,
                started_at: now,
            }),
        ];
        ranks.sort_unstable();
        assert_eq!(ranks, [0, 1, 2]);
    }

    #[test]
    fn states_read_as_the_operator_expects() {
        let now = Utc::now();
        let ago = now - chrono::Duration::minutes(7);
        assert_eq!(describe_state(&GroupState::NoSession, now), "no session");
        assert_eq!(describe_state(&GroupState::Idle, now), "no session");
        assert_eq!(
            describe_state(&GroupState::Starting { since: ago }, now),
            "starting"
        );
        assert_eq!(
            describe_state(
                &GroupState::Ready {
                    pane_pid: 1,
                    since: ago
                },
                now
            ),
            "ready"
        );
        assert_eq!(
            describe_state(
                &GroupState::Delivering {
                    since: ago,
                    event_id: 1
                },
                now
            ),
            "delivering"
        );
        // Who started the turn is the point of the label.
        assert_eq!(
            describe_state(
                &GroupState::Busy {
                    since: ago,
                    delivered_event: Some(1)
                },
                now
            ),
            "busy (agent) 7m"
        );
        assert_eq!(
            describe_state(
                &GroupState::Busy {
                    since: ago,
                    delivered_event: None
                },
                now
            ),
            "busy (you) 7m"
        );
    }

    #[test]
    fn an_empty_worktree_reads_as_absent_rather_than_blank() {
        assert_eq!(display_path(Path::new("")), "-");
        assert_eq!(display_path(Path::new("/w/irr")), "/w/irr");
    }

    #[test]
    fn columns_are_padded_to_their_widest_cell() {
        let rows = vec![
            vec!["GROUP".to_string(), "STATE".to_string()],
            vec!["a-very-long-group".to_string(), "idle".to_string()],
            vec!["irr".to_string(), "running 2m".to_string()],
        ];
        let mut buf: Vec<u8> = Vec::new();
        write_table(&mut buf, &rows).expect("write");
        let text = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines[0], "GROUP              STATE");
        assert_eq!(lines[1], "a-very-long-group  idle");
        assert_eq!(lines[2], "irr                running 2m");
        // No trailing whitespace, so the output survives a copy-paste.
        assert!(lines.iter().all(|l| l == &l.trim_end()));
    }
}
