//! Operator controls for the dispatcher's sessions.
//!
//! `conductor serve` runs unattended. These subcommands are how a human sees
//! what it is doing and steps in: take a session over interactively, stop a
//! runaway run, force a fresh conversation, or retire a finished group.
//!
//! They run inside the pod via `kubectl exec`, against the same registry the
//! server writes to. Coordination is entirely through Postgres: every mutation
//! is one locked transaction, and the dispatcher re-scans periodically, so a
//! change made here takes effect without any IPC channel between the two.
//!
//! The lock is held only for the read-modify-write. Waiting for a run to end,
//! killing a process and the interactive session itself all happen with the
//! lock released, because holding it would block the server that is trying to
//! make progress.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::Subcommand;
use nix::sys::signal::Signal;

use crate::dispatcher::{ensure_worktree, run_git, signal_process, SessionEnv};
use crate::registry::{
    process_is_alive, recorded_start_time, Group, GroupKey, GroupState, QueuedEvent, Registry,
    RunOutcome, RunResult,
};
use crate::InboundEvent;

/// Exit code for a group that does not exist.
const EXIT_NO_SUCH_GROUP: i32 = 2;
/// Exit code for a refusal the operator can override with `--force`.
const EXIT_REFUSED: i32 = 1;
/// Exit code for an interrupted wait, by convention `128 + SIGINT`.
const EXIT_INTERRUPTED: i32 = 130;

/// How often to re-check a process we signalled but cannot `wait()` for.
const DEATH_POLL: Duration = Duration::from_millis(100);

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
    },

    /// Take over a session interactively.
    ///
    /// Waits for any dispatched run to finish first, then starts an interactive
    /// Claude in the group's worktree. The group is marked `attached` for the
    /// duration, so the dispatcher leaves it alone and queues its events.
    ///
    /// A dropped `kubectl exec` sends SIGHUP; that is forwarded to Claude and
    /// the group is still returned to `idle`. If this command is SIGKILLed
    /// instead, nothing can run on exit, and the group is recovered by the
    /// registry's stale-process check the next time the server loads it.
    Attach {
        /// Group to attach to, as shown by `sessions list`.
        group: String,
        /// Stop whatever is running first, instead of waiting for it.
        #[arg(long)]
        force: bool,
    },

    /// Stop a group's running process.
    Kill {
        /// Group to stop.
        group: String,
        /// Also discard the group's queued events.
        #[arg(long)]
        drain: bool,
    },

    /// Start a fresh Claude conversation for a group, keeping its worktree.
    Reset {
        /// Group to reset.
        group: String,
        /// Stop a running process first, instead of refusing.
        #[arg(long)]
        force: bool,
    },

    /// Retire a group: remove its worktree and forget it.
    ///
    /// Branches are left alone. A later event for the same key builds the group
    /// again from scratch.
    Close {
        /// Group to close.
        group: String,
        /// Proceed despite a running process, queued events or a dirty worktree.
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
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    match command {
        SessionsCommand::List { json } => list(registry, out, json).await,
        SessionsCommand::Attach { group, force } => attach(&group, force, env, registry, out).await,
        SessionsCommand::Kill { group, drain } => kill(&group, drain, env, registry, out).await,
        SessionsCommand::Reset { group, force } => reset(&group, force, env, registry, out).await,
        SessionsCommand::Close { group, force } => close(&group, force, env, registry, out).await,
    }
}

// ── list ─────────────────────────────────────────────────────────────────────

async fn list(
    registry: &Registry,
    out: &mut (dyn Write + Send),
    json: bool,
) -> Result<i32, SessionsError> {
    // Unlocked read, so this works while the server is mid-write.
    let mut groups = registry.snapshot().await?;

    if json {
        let body = serde_json::to_string_pretty(&groups)
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
        "PENDING",
        "LAST ISSUE",
        "LAST ACTIVE",
        "LAST RUN",
        "SESSION",
        "WORKTREE",
    ];
    let mut rows: Vec<Vec<String>> = vec![header.iter().map(|h| h.to_string()).collect()];

    for g in &groups {
        rows.push(vec![
            g.key.slug(),
            describe_state(&g.state, now),
            g.pending.len().to_string(),
            g.issues.last().cloned().unwrap_or_else(|| "-".into()),
            format!("{} ago", human_duration(now - g.last_active)),
            describe_outcome(g.last_run.as_ref()),
            // Enough to correlate with a log line without filling the row.
            g.session_id.to_string()[..8].to_string(),
            display_path(&g.worktree),
        ]);
    }

    write_table(out, &rows)?;
    Ok(0)
}

/// Sort key putting running groups first, then attached, then idle.
fn state_rank(state: &GroupState) -> u8 {
    match state {
        GroupState::Running { .. } => 0,
        GroupState::Attached { .. } => 1,
        GroupState::Idle => 2,
    }
}

fn describe_state(state: &GroupState, now: DateTime<Utc>) -> String {
    match state {
        GroupState::Idle => "idle".into(),
        GroupState::Running { started_at, .. } => {
            format!("running {}", human_duration(now - *started_at))
        }
        GroupState::Attached { .. } => "attached".into(),
    }
}

fn describe_outcome(outcome: Option<&RunOutcome>) -> String {
    let Some(o) = outcome else {
        return "-".into();
    };
    match &o.result {
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

async fn attach(
    slug: &str,
    force: bool,
    env: &SessionEnv,
    registry: &Registry,
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    let Some(group) = lookup(registry, slug, out).await? else {
        return Ok(EXIT_NO_SUCH_GROUP);
    };
    let key = group.key.clone();

    // Before anything is claimed, so there is no moment where a hangup can
    // kill this process while the registry says the group is attached.
    let mut signals = StopSignals::install()?;

    // ── 1. get the group to ourselves ───────────────────────────────────────
    //
    // Looped rather than checked once: between a run ending and this process
    // claiming the group, the dispatcher may start the next queued event. With
    // `--force` that run is stopped too, which is what an operator asking to
    // force their way in means.
    let group = loop {
        let Some(current) = registry.group_by_slug(slug).await? else {
            writeln!(out, "group {slug} disappeared while waiting")?;
            return Ok(EXIT_NO_SUCH_GROUP);
        };

        match current.state {
            GroupState::Idle => {}
            GroupState::Attached { pid, .. } => {
                writeln!(
                    out,
                    "{slug} is already attached (pid {pid}); detach there first"
                )?;
                return Ok(EXIT_REFUSED);
            }
            GroupState::Running { pid, started_at } if force => {
                writeln!(
                    out,
                    "stopping current run (pid {pid}, started {} ago)…",
                    human_duration(Utc::now() - started_at)
                )?;
                out.flush()?;
                stop_process(registry, &key, pid, started_at, env, "attach --force").await?;
                continue;
            }
            GroupState::Running { pid, started_at } => {
                writeln!(
                    out,
                    "waiting for current run (pid {pid}, started {} ago)…",
                    human_duration(Utc::now() - started_at)
                )?;
                out.flush()?;
                // Ctrl-C here leaves the registry exactly as it was.
                if wait_or_interrupt(env.poll_interval, &mut signals)
                    .await
                    .is_err()
                {
                    writeln!(out, "interrupted; nothing was changed")?;
                    return Ok(EXIT_INTERRUPTED);
                }
                continue;
            }
        }

        // Claim it. The read above is advisory — this transaction re-checks
        // under the row lock, and only one claimant can win.
        let my_pid = std::process::id();
        let started_at = recorded_start_time(my_pid);
        let mut txn = registry.begin().await?;
        let claimed = txn.attach(&key, my_pid, started_at).await?;
        if !claimed {
            // Something took it between the read and here. Look again, after a
            // pause: without one, a group that keeps being claimed would turn
            // this into a tight loop against the database.
            txn.commit().await?;
            if wait_or_interrupt(env.poll_interval, &mut signals)
                .await
                .is_err()
            {
                writeln!(out, "interrupted; nothing was changed")?;
                return Ok(EXIT_INTERRUPTED);
            }
            continue;
        }
        let group = txn
            .group_by_slug(slug)
            .await?
            .ok_or_else(|| SessionsError::Failed(format!("group {slug} vanished mid-claim")))?;
        if !group.session_started {
            txn.mark_session_started(&key).await?;
        }
        txn.commit().await?;
        break group;
    };

    // ── 2. run, and give the group back whatever happens ────────────────────
    //
    // Past this point the group is marked attached, so every exit path has to
    // release it or the dispatcher will never touch the group again.
    let outcome = run_interactive(&group, env, registry, out, &mut signals).await;

    let mut txn = registry.begin().await?;
    txn.release(&key).await?;
    txn.commit().await?;

    match outcome {
        Ok(code) => {
            writeln!(out, "detached; {slug} is idle again")?;
            Ok(code)
        }
        Err(e) => {
            writeln!(out, "attach failed: {e}")?;
            Ok(EXIT_REFUSED)
        }
    }
}

/// Prepares the worktree, then runs an interactive Claude to completion.
async fn run_interactive(
    group: &Group,
    env: &SessionEnv,
    registry: &Registry,
    out: &mut (dyn Write + Send),
    signals: &mut StopSignals,
) -> Result<i32, SessionsError> {
    let slug = group.key.slug();

    // A group can exist with queued events and no checkout yet, if it was
    // created while the dispatcher was at its concurrency cap.
    //
    // The server serialises its own worktree creation with an in-process lock,
    // which does not reach across to this process. The window is one operator
    // command against a server that happens to be preparing another worktree
    // at that instant, and it surfaces as a clear git error and an unchanged
    // group rather than as damage; a cross-process lock would be the fix if it
    // ever actually bites.
    let base_ref = registry.worktree_ref(&group.key).await?;
    let worktree = ensure_worktree(
        &env.repo_root,
        &env.worktree_root,
        &slug,
        base_ref.as_deref(),
    )
    .await
    .map_err(|e| SessionsError::Failed(e.to_string()))?;

    if worktree != group.worktree {
        let mut txn = registry.begin().await?;
        txn.set_worktree(&group.key, &worktree).await?;
        txn.commit().await?;
    }

    let mut command = tokio::process::Command::new(&env.claude_bin);
    command.current_dir(&worktree);
    if group.session_started {
        command.arg("--resume").arg(group.session_id.to_string());
    } else {
        command
            .arg("--session-id")
            .arg(group.session_id.to_string());
    }
    // The agent the session was dispatched under. Resuming without it would
    // drop the instructions the conversation has been following.
    command.arg(format!("--agent={}", env.agent));
    // Deliberately no `--print` and no `--dangerously-skip-permissions`: this
    // is an interactive session with a human at the keyboard, who can answer
    // permission prompts themselves.
    command.env("CARGO_TARGET_DIR", worktree.join("target"));
    // stdio is inherited by default, which is what makes the session usable.

    writeln!(
        out,
        "attached to {slug} in {} (session {})",
        worktree.display(),
        &group.session_id.to_string()[..8]
    )?;
    out.flush()?;

    let mut child = command
        .spawn()
        .map_err(|e| SessionsError::Failed(format!("starting {}: {e}", env.claude_bin)))?;
    let child_pid = child.id().unwrap_or_default();

    wait_forwarding_signals(&mut child, child_pid, signals).await
}

/// The signals that mean "stop", installed once and shared by both phases.
///
/// Installed *before* the group is claimed, which closes a window that was
/// otherwise real: the handlers used to go up only once Claude had been
/// spawned, so a hangup arriving while the worktree was still being prepared
/// killed this process at its default action and left the group stuck in
/// `attached` with nothing running. A dropped `kubectl exec` during a slow
/// `git fetch` is exactly when that would happen.
struct StopSignals {
    hangup: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

impl StopSignals {
    fn install() -> Result<Self, SessionsError> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(StopSignals {
            hangup: signal(SignalKind::hangup())?,
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    /// Resolves with the first stop signal to arrive.
    async fn recv(&mut self) -> Signal {
        tokio::select! {
            _ = self.hangup.recv() => Signal::SIGHUP,
            _ = self.terminate.recv() => Signal::SIGTERM,
            _ = self.interrupt.recv() => Signal::SIGINT,
        }
    }
}

/// Waits for the child, passing on any signal that means "stop".
///
/// SIGHUP is the one that matters in practice: a dropped `kubectl exec`
/// connection sends it, and without forwarding, Claude would be left running
/// with nobody attached. The wait continues afterwards in every case, so the
/// caller still reaches the release once the child is actually gone.
async fn wait_forwarding_signals(
    child: &mut tokio::process::Child,
    child_pid: u32,
    signals: &mut StopSignals,
) -> Result<i32, SessionsError> {
    loop {
        tokio::select! {
            status = child.wait() => {
                return Ok(status?.code().unwrap_or(EXIT_REFUSED));
            }
            sig = signals.recv() => {
                tracing::info!(pid = child_pid, ?sig, "forwarding a stop signal to the attached session");
                signal_process(child_pid, sig);
            }
        }
    }
}

/// Sleeps, unless a stop signal arrives first.
///
/// `Err(())` means the operator gave up — Ctrl-C, or the connection dropping.
/// Nothing has been claimed at this point, so returning leaves the registry
/// exactly as it was.
async fn wait_or_interrupt(period: Duration, signals: &mut StopSignals) -> Result<(), ()> {
    tokio::select! {
        _ = tokio::time::sleep(period) => Ok(()),
        _ = signals.recv() => Err(()),
    }
}

// ── kill ─────────────────────────────────────────────────────────────────────

async fn kill(
    slug: &str,
    drain: bool,
    env: &SessionEnv,
    registry: &Registry,
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    let Some(group) = lookup(registry, slug, out).await? else {
        return Ok(EXIT_NO_SUCH_GROUP);
    };

    match group.state {
        GroupState::Idle => {
            writeln!(out, "{slug} is not running")?;
        }
        GroupState::Running { pid, started_at } | GroupState::Attached { pid, started_at } => {
            writeln!(out, "stopping pid {pid}…")?;
            out.flush()?;
            stop_process(registry, &group.key, pid, started_at, env, "cli").await?;
            writeln!(out, "{slug} stopped")?;
        }
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
            "{queued} queued event(s) still waiting; they run on the next scan"
        )?;
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
    force: bool,
    env: &SessionEnv,
    registry: &Registry,
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    let Some(group) = lookup(registry, slug, out).await? else {
        return Ok(EXIT_NO_SUCH_GROUP);
    };

    if let Some((pid, started_at)) = busy(&group.state) {
        if !force {
            writeln!(
                out,
                "{slug} is {} (pid {pid}); use --force to stop it first",
                state_word(&group.state)
            )?;
            return Ok(EXIT_REFUSED);
        }
        writeln!(out, "stopping pid {pid}…")?;
        out.flush()?;
        stop_process(registry, &group.key, pid, started_at, env, "cli").await?;
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
    out: &mut (dyn Write + Send),
) -> Result<i32, SessionsError> {
    let Some(group) = lookup(registry, slug, out).await? else {
        return Ok(EXIT_NO_SUCH_GROUP);
    };

    if let Some((pid, started_at)) = busy(&group.state) {
        if !force {
            writeln!(
                out,
                "{slug} is {} (pid {pid}); use --force to stop it and close anyway",
                state_word(&group.state)
            )?;
            return Ok(EXIT_REFUSED);
        }
        writeln!(out, "stopping pid {pid}…")?;
        out.flush()?;
        stop_process(registry, &group.key, pid, started_at, env, "cli").await?;
    }

    if !group.pending.is_empty() && !force {
        writeln!(
            out,
            "{slug} has {} queued event(s); use --force to discard them and close",
            group.pending.len()
        )?;
        return Ok(EXIT_REFUSED);
    }

    // Git first: a failure here must leave the group intact, or the registry
    // would forget a worktree that is still on disk.
    if !group.worktree.as_os_str().is_empty() && group.worktree.exists() {
        let path = group.worktree.to_string_lossy().to_string();
        let args: Vec<&str> = if force {
            vec!["worktree", "remove", "--force", &path]
        } else {
            vec!["worktree", "remove", &path]
        };
        if let Err(e) = run_git(&env.repo_root, &args).await {
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
        writeln!(out, "removed worktree {path}")?;
    }

    let mut txn = registry.begin().await?;
    let removed = txn.delete_group(&group.key).await?;
    txn.commit().await?;

    if !removed {
        writeln!(out, "{slug} was already gone")?;
        return Ok(0);
    }

    writeln!(out, "closed {slug}")?;
    if group.branches.is_empty() {
        writeln!(out, "no branches were recorded for it")?;
    } else {
        // Left behind deliberately: the work on them may not be merged yet.
        writeln!(out, "these branches are left in place:")?;
        for branch in &group.branches {
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

/// The process backing a group, when it has one.
fn busy(state: &GroupState) -> Option<(u32, DateTime<Utc>)> {
    match state {
        GroupState::Idle => None,
        GroupState::Running { pid, started_at } | GroupState::Attached { pid, started_at } => {
            Some((*pid, *started_at))
        }
    }
}

fn state_word(state: &GroupState) -> &'static str {
    match state {
        GroupState::Idle => "idle",
        GroupState::Running { .. } => "running",
        GroupState::Attached { .. } => "attached",
    }
}

/// `SIGTERM`, then `SIGKILL` after the grace, then record the outcome.
///
/// The process belongs to `conductor serve`, not to us, so it cannot be
/// `wait()`ed for — liveness is polled with the same start-time-aware check the
/// registry uses, which is what stops a recycled PID being read as "still
/// running" and leaving us waiting on a stranger's process.
///
/// The outcome is written after the process is confirmed gone. If the server is
/// also running it will record its own view of the same death as it reaps the
/// child; whichever transaction commits last wins, so the recorded outcome may
/// be the server's `signaled 15` rather than this `killed`. The group ends up
/// `Idle` either way, which is the part that matters.
async fn stop_process(
    registry: &Registry,
    key: &GroupKey,
    pid: u32,
    started_at: DateTime<Utc>,
    env: &SessionEnv,
    by: &str,
) -> Result<(), SessionsError> {
    signal_process(pid, Signal::SIGTERM);

    if !await_death(pid, started_at, env.kill_grace).await {
        tracing::warn!(pid, "process ignored SIGTERM; sending SIGKILL");
        signal_process(pid, Signal::SIGKILL);
        // SIGKILL cannot be caught, so this only waits out the kernel.
        await_death(pid, started_at, env.kill_grace).await;
    }

    let mut txn = registry.begin().await?;
    let now = txn.now();
    txn.finish_run(
        key,
        &RunOutcome {
            result: RunResult::Killed { by: by.to_string() },
            finished_at: now,
            duration: (now - started_at).to_std().unwrap_or_default(),
        },
    )
    .await?;
    txn.commit().await?;
    Ok(())
}

/// Polls until the process is gone, or the deadline passes. Returns whether it died.
async fn await_death(pid: u32, started_at: DateTime<Utc>, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if !process_is_alive(pid, started_at) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(DEATH_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(result: RunResult) -> RunOutcome {
        RunOutcome {
            result,
            finished_at: Utc::now(),
            duration: Duration::from_secs(1),
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
    fn a_running_state_shows_how_long_it_has_been_going() {
        let now = Utc::now();
        let state = GroupState::Running {
            pid: 42,
            started_at: now - chrono::Duration::minutes(7),
        };
        assert_eq!(describe_state(&state, now), "running 7m");
        assert_eq!(describe_state(&GroupState::Idle, now), "idle");
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
