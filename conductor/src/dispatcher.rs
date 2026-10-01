//! Dispatcher: routes accepted events to Claude Code sessions.
//!
//! Runs serially within a session group and in parallel across groups, up to a
//! global cap. Each group works in its own git worktree, so parallel sessions
//! never share a checkout.
//!
//! Replaces the previous arrangement, where every event resumed one hardcoded
//! session in the background — two events close together produced two
//! concurrent resumes of the same conversation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex, Notify, Semaphore};

use crate::github::PrCommentKind;
use crate::registry::{GroupKey, QueuedEvent, Registry, RunOutcome, RunResult};
use crate::resolve::{fallback_key, GroupResolver};
use crate::{EventSink, InboundEvent};

/// How long the scheduler waits before re-scanning when nothing wakes it.
///
/// The CLI mutates the registry from another process, so polling is how those
/// changes take effect without inventing an IPC channel.
const RESCAN_INTERVAL: Duration = Duration::from_secs(5);

/// Grace between `SIGTERM` and `SIGKILL` for a run being stopped.
const TERM_TO_KILL: Duration = Duration::from_secs(30);

/// Dispatcher configuration, parsed by `clap` from flags or environment.
///
/// Flattened into `conductor serve`, so every setting is both a documented flag
/// and an environment variable, and a malformed value is reported at startup
/// rather than silently replaced by a default.
#[derive(Debug, Clone, clap::Args)]
pub struct DispatchArgs {
    /// Maximum concurrent runs across all groups.
    #[arg(long, env = "CONDUCTOR_MAX_SESSIONS", default_value_t = 5, value_parser = clap::value_parser!(u16).range(1..))]
    pub max_sessions: u16,

    /// Repository the worktrees are added from. Defaults to the current directory.
    #[arg(long, env = "CONDUCTOR_REPO_ROOT")]
    pub repo_root: Option<PathBuf>,

    /// Directory holding one worktree per group. Defaults to `<state-dir>/worktrees`.
    #[arg(long, env = "CONDUCTOR_WORKTREE_ROOT")]
    pub worktree_root: Option<PathBuf>,

    /// Directory holding per-run logs. Defaults to `~/.local/state/conductor`.
    #[arg(long, env = "CONDUCTOR_STATE_DIR")]
    pub state_dir: Option<PathBuf>,

    /// Binary to execute; overridable so tests need no real Claude.
    #[arg(long, env = "CONDUCTOR_CLAUDE_BIN", default_value = "claude")]
    pub claude_bin: String,

    /// Agent passed through to Claude.
    #[arg(long, env = "CONDUCTOR_AGENT", default_value = "backend")]
    pub agent: String,

    /// Pass `--dangerously-skip-permissions` to Claude.
    #[arg(long, env = "CONDUCTOR_SKIP_PERMISSIONS", default_value_t = true, action = clap::ArgAction::Set)]
    pub skip_permissions: bool,

    /// How long a run may take, in minutes.
    #[arg(long, env = "CONDUCTOR_RUN_TIMEOUT_MIN", default_value_t = 120)]
    pub run_timeout_min: u64,

    /// Run timeout in seconds. Takes precedence, so tests need not wait minutes.
    #[arg(long, env = "CONDUCTOR_RUN_TIMEOUT_SECS")]
    pub run_timeout_secs: Option<u64>,

    /// How long shutdown waits for runs to finish, in seconds.
    #[arg(long, env = "CONDUCTOR_SHUTDOWN_GRACE_SECS", default_value_t = 600)]
    pub shutdown_grace_secs: u64,

    /// GitHub API base URL. Overridable so tests can point at a mock.
    #[arg(
        long,
        env = "CONDUCTOR_GITHUB_API_URL",
        default_value = "https://api.github.com"
    )]
    pub github_api_url: String,

    /// Token used to look up a pull request's head branch.
    ///
    /// Absent in local development, where the lookup is skipped and comments go
    /// to a per-pull-request group instead.
    #[arg(long, env = "GITHUB_TOKEN")]
    pub github_token: Option<String>,
}

impl DispatchArgs {
    /// Resolves the settings whose defaults depend on other settings.
    pub fn into_config(self) -> DispatchConfig {
        let state_dir = self.state_dir.unwrap_or_else(default_state_dir);
        DispatchConfig {
            max_sessions: usize::from(self.max_sessions),
            repo_root: self
                .repo_root
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into())),
            worktree_root: self
                .worktree_root
                .unwrap_or_else(|| state_dir.join("worktrees")),
            state_dir,
            claude_bin: self.claude_bin,
            agent: self.agent,
            skip_permissions: self.skip_permissions,
            // Seconds win when given, so a test can use a timeout far under a minute.
            run_timeout: Duration::from_secs(
                self.run_timeout_secs
                    .unwrap_or_else(|| self.run_timeout_min.saturating_mul(60)),
            ),
            shutdown_grace: Duration::from_secs(self.shutdown_grace_secs),
        }
    }
}

/// Where logs and worktrees live when nothing says otherwise.
fn default_state_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join(".local/state/conductor")
}

/// Resolved dispatcher configuration.
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    /// Maximum concurrent runs across all groups.
    pub max_sessions: usize,
    /// Repository the worktrees are added from.
    pub repo_root: PathBuf,
    /// Directory holding one worktree per group.
    pub worktree_root: PathBuf,
    /// Directory holding per-run logs.
    pub state_dir: PathBuf,
    /// Binary to execute.
    pub claude_bin: String,
    /// Agent passed through to Claude.
    pub agent: String,
    /// Whether to pass `--dangerously-skip-permissions`.
    pub skip_permissions: bool,
    /// How long a run may take before it is stopped.
    pub run_timeout: Duration,
    /// How long shutdown waits for runs to finish.
    pub shutdown_grace: Duration,
}

/// Stub resolver giving every pull request its own group.
///
/// Kept for tests and local use: it needs no GitHub token and makes no network
/// call. The real resolver lives in [`crate::resolve::BranchResolver`].
pub struct PerPullRequestResolver;

#[async_trait::async_trait]
impl GroupResolver for PerPullRequestResolver {
    async fn resolve(&self, event: &InboundEvent) -> Option<GroupKey> {
        match event {
            InboundEvent::GithubPrComment { comment, .. } => {
                Some(fallback_key(&comment.repo, comment.pr_number))
            }
            // Linear events already carry their own key.
            other => Some(crate::registry::group_key_for(other)),
        }
    }
}

/// Accepts events from the HTTP layer and hands them to the dispatcher.
///
/// Sending on a channel rather than doing the work inline keeps the webhook
/// response off the critical path.
pub struct DispatchSink {
    tx: mpsc::UnboundedSender<InboundEvent>,
}

impl EventSink for DispatchSink {
    fn submit(&self, event: InboundEvent) {
        if self.tx.send(event).is_err() {
            tracing::error!("dispatcher is gone; dropping event");
        }
    }
}

/// A run in progress, kept in memory for timeout and shutdown.
struct RunHandle {
    pid: u32,
}

/// Owns the scheduler, the worktrees and the running children.
pub struct Dispatcher {
    registry: Registry,
    config: DispatchConfig,
    resolver: Arc<dyn GroupResolver>,
    permits: Arc<Semaphore>,
    wake: Arc<Notify>,
    running: Arc<Mutex<HashMap<String, RunHandle>>>,
    shutting_down: Arc<AtomicBool>,
}

impl Dispatcher {
    /// Builds a dispatcher and the sink feeding it.
    pub fn new(
        registry: Registry,
        config: DispatchConfig,
        resolver: Arc<dyn GroupResolver>,
        shutting_down: Arc<AtomicBool>,
    ) -> (
        Arc<Self>,
        DispatchSink,
        mpsc::UnboundedReceiver<InboundEvent>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let dispatcher = Arc::new(Dispatcher {
            permits: Arc::new(Semaphore::new(config.max_sessions)),
            registry,
            config,
            resolver,
            wake: Arc::new(Notify::new()),
            running: Arc::new(Mutex::new(HashMap::new())),
            shutting_down,
        });
        (dispatcher, DispatchSink { tx }, rx)
    }

    /// Persists an accepted event and wakes the scheduler.
    ///
    /// Resolution happens before the lock is taken, so a slow lookup never
    /// holds up other writers.
    async fn ingest(&self, event: InboundEvent) {
        // Routing may need a network call, so it happens before any lock.
        let Some(key) = self.resolver.resolve(&event).await else {
            tracing::info!("routing declined this event; nothing queued");
            return;
        };

        // Also resolved up front: a group created for someone else's pull
        // request must start on that pull request's branch, and finding that out
        // can cost a request. Served from the resolver's cache in practice.
        let base_ref = match &event {
            InboundEvent::GithubPrComment { comment, .. }
                if key == fallback_key(&comment.repo, comment.pr_number) =>
            {
                self.resolver.worktree_ref(&event).await
            }
            _ => None,
        };

        let delivery = delivery_id(&event).map(str::to_string);

        let mut txn = match self.registry.begin().await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "cannot reach the registry; event dropped");
                return;
            }
        };

        let result = async {
            if !txn.mark_delivery_seen(delivery.as_deref()).await? {
                tracing::info!(delivery_id = ?delivery, "duplicate delivery ignored");
                return Ok(false);
            }
            // Creates the group if absent.
            txn.session_for_dispatch(&key).await?;
            if let Some(base) = &base_ref {
                txn.set_worktree_ref_if_absent(&key, base).await?;
            }
            txn.enqueue(&key, &event).await?;
            Ok::<_, sqlx::Error>(true)
        }
        .await;

        match result {
            Ok(queued) => {
                if let Err(e) = txn.commit().await {
                    tracing::error!(error = %e, "failed to persist event");
                    return;
                }
                if queued {
                    tracing::info!(group = %key.slug(), "event queued");
                    self.wake.notify_one();
                }
            }
            Err(e) => tracing::error!(error = %e, "failed to queue event"),
        }
    }

    /// Receives submitted events until the sink is dropped.
    pub async fn run_ingest(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<InboundEvent>) {
        while let Some(event) = rx.recv().await {
            self.ingest(event).await;
        }
    }

    /// The scheduler: starts runnable groups, then waits to be woken or rescans.
    pub async fn run_scheduler(self: Arc<Self>) {
        // Anything left Running by a previous process is already reset by the
        // registry's stale-PID recovery; schedule whatever it freed up.
        match self.registry.reconcile_stale_processes().await {
            Ok(n) if n > 0 => tracing::warn!(groups = n, "reset runs lost with a previous process"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "stale process recovery failed"),
        }

        loop {
            if self.shutting_down.load(Ordering::SeqCst) {
                tracing::info!("scheduler stopping; no new runs will start");
                return;
            }

            if let Err(e) = self.start_runnable().await {
                tracing::error!(error = %e, "scheduling pass failed");
            }

            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(RESCAN_INTERVAL) => {}
            }
        }
    }

    /// Starts as many runnable groups as permits allow.
    async fn start_runnable(self: &Arc<Self>) -> Result<(), sqlx::Error> {
        let mut txn = self.registry.begin().await?;
        let keys = txn.runnable_groups().await?;
        txn.commit().await?;

        for key in keys {
            if self.shutting_down.load(Ordering::SeqCst) {
                return Ok(());
            }
            // Non-blocking: if the cap is reached, leave the rest queued.
            let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
                return Ok(());
            };

            let own_pid = std::process::id();
            let mut txn = self.registry.begin().await?;
            let claimed = txn.claim_next(&key, own_pid, Utc::now()).await?;
            txn.commit().await?;

            let Some(queued) = claimed else {
                // Another writer took it, or it went busy.
                drop(permit);
                continue;
            };

            let me = Arc::clone(self);
            tokio::spawn(async move {
                me.run_one(key, queued).await;
                drop(permit);
            });
        }
        Ok(())
    }

    /// Executes exactly one event, then lets the scheduler re-evaluate.
    async fn run_one(self: Arc<Self>, key: GroupKey, queued: QueuedEvent) {
        let slug = key.slug();
        let started = Instant::now();

        let outcome = match self.execute(&key, &queued).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::error!(group = %slug, error = %e, "run could not start");
                RunResult::Exited { code: -1 }
            }
        };

        let duration = started.elapsed();
        tracing::info!(
            group = %slug,
            issue = ?issue_identifier(&queued.event),
            outcome = ?outcome,
            duration_secs = duration.as_secs(),
            "run finished"
        );

        self.running.lock().await.remove(&slug);

        if let Err(e) = self
            .record_completion(&key, &queued, outcome, duration)
            .await
        {
            tracing::error!(group = %slug, error = %e, "failed to record run outcome");
        }

        // A failed run is recorded, never retried: re-running half-finished work
        // can open a duplicate pull request. The next queued event runs normally.
        self.wake.notify_one();
    }

    /// Prepares the worktree, spawns the child and waits for it.
    async fn execute(
        &self,
        key: &GroupKey,
        queued: &QueuedEvent,
    ) -> Result<RunResult, Box<dyn std::error::Error + Send + Sync>> {
        let slug = key.slug();
        let base_ref = self.registry.worktree_ref(key).await?;
        let worktree = self.ensure_worktree(&slug, base_ref.as_deref()).await?;

        let mut txn = self.registry.begin().await?;
        txn.set_worktree(key, &worktree).await?;
        let (session_id, resume) = txn.session_for_dispatch(key).await?;
        txn.commit().await?;

        let log_path = self.log_path(&slug, &queued.event)?;
        let log = std::fs::File::create(&log_path)?;
        let log_err = log.try_clone()?;

        let prompt = prompt_for(&queued.event);
        let mut command = Command::new(&self.config.claude_bin);
        command.current_dir(&worktree);
        if resume {
            command.arg("--resume").arg(session_id.to_string());
        } else {
            command.arg("--session-id").arg(session_id.to_string());
        }
        command.arg(format!("--agent={}", self.config.agent));
        if self.config.skip_permissions {
            command.arg("--dangerously-skip-permissions");
        }
        command.arg("--print").arg(&prompt);
        // Each worktree needs its own build cache; a shared target directory
        // would serialise parallel sessions on cargo's lock.
        command.env("CARGO_TARGET_DIR", worktree.join("target"));
        command
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        command.kill_on_drop(true);

        tracing::info!(
            group = %slug,
            issue = ?issue_identifier(&queued.event),
            resume,
            log = %log_path.display(),
            "run starting"
        );

        let mut child = command.spawn()?;
        let pid = child.id().unwrap_or_default();

        let mut txn = self.registry.begin().await?;
        txn.set_running_process(key, pid, Utc::now()).await?;
        txn.mark_session_started(key).await?;
        txn.commit().await?;

        self.running
            .lock()
            .await
            .insert(slug.clone(), RunHandle { pid });

        Ok(self.wait_with_timeout(&mut child, pid, &slug).await)
    }

    /// Waits for the child, stopping it if it outlives the timeout.
    async fn wait_with_timeout(&self, child: &mut Child, pid: u32, slug: &str) -> RunResult {
        match tokio::time::timeout(self.config.run_timeout, child.wait()).await {
            Ok(Ok(status)) => exit_to_result(status),
            Ok(Err(e)) => {
                tracing::error!(group = %slug, error = %e, "waiting on the run failed");
                RunResult::Exited { code: -1 }
            }
            Err(_) => {
                tracing::warn!(
                    group = %slug,
                    timeout_secs = self.config.run_timeout.as_secs(),
                    "run exceeded its timeout; stopping it"
                );
                terminate(pid, child).await;
                RunResult::TimedOut
            }
        }
    }

    /// Writes the outcome and the history the next run depends on.
    async fn record_completion(
        &self,
        key: &GroupKey,
        queued: &QueuedEvent,
        result: RunResult,
        duration: Duration,
    ) -> Result<(), sqlx::Error> {
        let branch = self.current_branch(&key.slug()).await;

        let mut txn = self.registry.begin().await?;
        txn.finish_run(
            key,
            &RunOutcome {
                result,
                finished_at: txn.now(),
                duration,
            },
        )
        .await?;
        if let Some(issue) = issue_identifier(&queued.event) {
            txn.append_issue(key, &issue).await?;
        }
        if let Some(branch) = branch {
            txn.record_branch(key, &branch).await?;
        }
        txn.commit().await
    }

    /// Creates the group's worktree, or reuses it if it is already one.
    async fn ensure_worktree(
        &self,
        slug: &str,
        base_ref: Option<&str>,
    ) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
        // The slug is validated on the way in, so it cannot escape this root.
        let path = self.config.worktree_root.join(slug);

        if path.exists() {
            if path.join(".git").exists() {
                return Ok(path);
            }
            // Deleting whatever this is would risk destroying real work.
            return Err(format!(
                "{} exists but is not a git worktree; refusing to touch it",
                path.display()
            )
            .into());
        }

        std::fs::create_dir_all(&self.config.worktree_root)?;

        // A group answering comments on an existing pull request works on that
        // pull request's branch; starting a fresh branch from main would have the
        // session editing code the review was not about.
        if let Some(base) = base_ref {
            run_git(&self.config.repo_root, &["fetch", "origin", base]).await?;
            let path_str = path.to_string_lossy().to_string();
            run_git(
                &self.config.repo_root,
                &["worktree", "add", &path_str, base],
            )
            .await?;
            tracing::info!(worktree = %path.display(), branch = %base, "worktree ready on pull request branch");
            return Ok(path);
        }

        run_git(&self.config.repo_root, &["fetch", "origin", "main"]).await?;

        let branch = format!("agent/{slug}");
        let branch_exists = run_git(
            &self.config.repo_root,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .await
        .is_ok();

        let path_str = path.to_string_lossy().to_string();
        let args: Vec<&str> = if branch_exists {
            // Reattaching an existing branch; -b would fail.
            vec!["worktree", "add", &path_str, &branch]
        } else {
            vec!["worktree", "add", &path_str, "-b", &branch, "origin/main"]
        };
        run_git(&self.config.repo_root, &args).await?;
        tracing::info!(worktree = %path.display(), branch = %branch, "worktree ready");
        Ok(path)
    }

    /// The branch currently checked out in the group's worktree.
    async fn current_branch(&self, slug: &str) -> Option<String> {
        let path = self.config.worktree_root.join(slug);
        let out = run_git(&path, &["branch", "--show-current"]).await.ok()?;
        let name = out.trim().to_string();
        (!name.is_empty()).then_some(name)
    }

    /// Per-run log file, one directory per group.
    fn log_path(&self, slug: &str, event: &InboundEvent) -> std::io::Result<PathBuf> {
        let dir = self.config.state_dir.join("logs").join(slug);
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join(format!(
            "{}-{}.log",
            Utc::now().timestamp(),
            event_name(event)
        )))
    }

    /// Stops accepting work and waits for runs to finish, then stops them.
    pub async fn shutdown(self: Arc<Self>) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
        tracing::info!(
            grace_secs = self.config.shutdown_grace.as_secs(),
            "shutting down; waiting for runs to finish"
        );

        let deadline = Instant::now() + self.config.shutdown_grace;
        while Instant::now() < deadline {
            if self.running.lock().await.is_empty() {
                tracing::info!("all runs finished");
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Whatever is left gets stopped. Its events are deliberately not
        // re-queued: re-running half-finished work can open a duplicate PR.
        let running = self.running.lock().await;
        for (slug, handle) in running.iter() {
            tracing::warn!(group = %slug, pid = handle.pid, "run interrupted by shutdown");
            signal(handle.pid, nix::sys::signal::Signal::SIGTERM);
        }
        drop(running);

        tokio::time::sleep(TERM_TO_KILL).await;
        for (slug, handle) in self.running.lock().await.iter() {
            tracing::warn!(group = %slug, pid = handle.pid, "run did not stop; killing it");
            signal(handle.pid, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

/// Sends a signal, ignoring a process that has already gone.
fn signal(pid: u32, sig: nix::sys::signal::Signal) {
    if pid == 0 || pid > i32::MAX as u32 {
        return;
    }
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), sig);
}

/// `SIGTERM`, then `SIGKILL` if the child is still there.
async fn terminate(pid: u32, child: &mut Child) {
    signal(pid, nix::sys::signal::Signal::SIGTERM);
    if tokio::time::timeout(TERM_TO_KILL, child.wait())
        .await
        .is_err()
    {
        signal(pid, nix::sys::signal::Signal::SIGKILL);
        let _ = child.wait().await;
    }
}

/// Runs git, returning stdout or the captured error.
async fn run_git(
    cwd: &Path,
    args: &[&str],
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Arguments are passed directly, never through a shell.
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Translates an exit status into a recordable outcome.
fn exit_to_result(status: std::process::ExitStatus) -> RunResult {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return RunResult::Signaled { signal };
        }
    }
    RunResult::Exited {
        code: status.code().unwrap_or(-1),
    }
}

/// The channel event name, matching what CLAUDE.md documents.
fn event_name(event: &InboundEvent) -> &'static str {
    match event {
        InboundEvent::LinearIssueTodo { .. } => "issue_todo",
        InboundEvent::GithubPrComment { kind, .. } => match kind {
            PrCommentKind::Comment => "pr_comment",
            PrCommentKind::ReviewComment => "pr_review_comment",
        },
    }
}

/// The prompt handed to Claude.
///
/// Event content is placed inside the channel envelope verbatim. It never
/// contributes to the command line, the working directory or the environment.
fn prompt_for(event: &InboundEvent) -> String {
    let body = match event {
        InboundEvent::LinearIssueTodo { issue, .. } => serde_json::to_string(issue),
        InboundEvent::GithubPrComment { comment, .. } => serde_json::to_string(comment),
    }
    .unwrap_or_else(|_| "{}".to_string());

    format!(
        "<channel source=\"webhook\" event=\"{}\">{}</channel>",
        event_name(event),
        body
    )
}

/// The Linear identifier an event refers to, if any.
fn issue_identifier(event: &InboundEvent) -> Option<String> {
    match event {
        InboundEvent::LinearIssueTodo { issue, .. } => Some(issue.identifier.clone()),
        InboundEvent::GithubPrComment { .. } => None,
    }
}

/// The provider's delivery identifier, used for idempotency.
fn delivery_id(event: &InboundEvent) -> Option<&str> {
    match event {
        InboundEvent::LinearIssueTodo { delivery_id, .. }
        | InboundEvent::GithubPrComment { delivery_id, .. } => delivery_id.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::GithubPrComment;
    use crate::linear::LinearIssue;

    fn linear(identifier: &str) -> InboundEvent {
        InboundEvent::LinearIssueTodo {
            issue: LinearIssue {
                identifier: identifier.into(),
                title: "t".into(),
                description: "d".into(),
                labels: vec![],
                session_labels: vec![],
            },
            delivery_id: Some("lin-1".into()),
        }
    }

    fn comment(repo: &str, pr: u64, kind: PrCommentKind) -> InboundEvent {
        InboundEvent::GithubPrComment {
            comment: GithubPrComment {
                repo: repo.into(),
                pr_number: pr,
                comment_id: 9,
                body: "b".into(),
                file_path: None,
                head_ref: None,
                head_repo: None,
                base_repo: None,
            },
            kind,
            delivery_id: None,
        }
    }

    #[test]
    fn event_names_match_the_documented_channel_names() {
        assert_eq!(event_name(&linear("NEX-1")), "issue_todo");
        assert_eq!(
            event_name(&comment("a/b", 1, PrCommentKind::Comment)),
            "pr_comment"
        );
        assert_eq!(
            event_name(&comment("a/b", 1, PrCommentKind::ReviewComment)),
            "pr_review_comment"
        );
    }

    #[test]
    fn prompt_wraps_the_event_in_the_channel_envelope() {
        let prompt = prompt_for(&linear("NEX-42"));
        assert!(prompt.starts_with("<channel source=\"webhook\" event=\"issue_todo\">"));
        assert!(prompt.ends_with("</channel>"));
        assert!(prompt.contains("NEX-42"));
    }

    /// Event content reaches an agent running with permissions skipped, so it
    /// must stay inside the envelope and never become an argument.
    #[test]
    fn prompt_is_one_argument_even_when_content_looks_like_flags() {
        let hostile = InboundEvent::LinearIssueTodo {
            issue: LinearIssue {
                identifier: "NEX-1".into(),
                title: "--dangerously-skip-permissions".into(),
                description: "`rm -rf /` $(whoami) --agent=evil".into(),
                labels: vec![],
                session_labels: vec![],
            },
            delivery_id: None,
        };
        let prompt = prompt_for(&hostile);
        // Serialised as JSON inside the envelope, not concatenated as arguments.
        assert!(prompt.contains("\\u002d\\u002ddangerously") || prompt.contains("--dangerously"));
        assert!(prompt.starts_with("<channel "));
        assert!(prompt.ends_with("</channel>"));
        // One string; the spawn path passes it via a single Command::arg.
        assert_eq!(prompt.matches("<channel ").count(), 1);
    }

    #[test]
    fn pull_request_groups_are_named_predictably() {
        let key = fallback_key("lassemand/nexus", 153);
        assert_eq!(key, GroupKey::Named("gh-lassemand-nexus-pr-153".into()));
        // And the name is a legal group key, so it can become a path.
        assert_eq!(key.slug(), "gh-lassemand-nexus-pr-153");
    }

    #[test]
    fn repo_names_are_reduced_to_safe_characters() {
        let key = fallback_key("Weird.Org/Repo_Name", 7);
        let GroupKey::Named(name) = key else {
            panic!("expected a named group");
        };
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "unsafe characters survived: {name}"
        );
    }

    #[tokio::test]
    async fn linear_events_keep_their_own_key() {
        // The stub only invents a key for events that lack one.
        let key = PerPullRequestResolver.resolve(&linear("NEX-42")).await;
        assert_eq!(key, Some(GroupKey::Issue("NEX-42".into())));
    }

    #[test]
    fn delivery_ids_are_read_from_either_variant() {
        assert_eq!(delivery_id(&linear("NEX-1")), Some("lin-1"));
        assert_eq!(
            delivery_id(&comment("a/b", 1, PrCommentKind::Comment)),
            None
        );
    }

    #[test]
    fn issue_identifier_is_only_present_for_linear() {
        assert_eq!(issue_identifier(&linear("NEX-7")).as_deref(), Some("NEX-7"));
        assert_eq!(
            issue_identifier(&comment("a/b", 1, PrCommentKind::Comment)),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn exit_status_maps_to_outcome() {
        use std::os::unix::process::ExitStatusExt;
        // Exit code 0 and 2.
        assert_eq!(
            exit_to_result(std::process::ExitStatus::from_raw(0)),
            RunResult::Exited { code: 0 }
        );
        assert_eq!(
            exit_to_result(std::process::ExitStatus::from_raw(2 << 8)),
            RunResult::Exited { code: 2 }
        );
        // Killed by SIGKILL (9).
        assert_eq!(
            exit_to_result(std::process::ExitStatus::from_raw(9)),
            RunResult::Signaled { signal: 9 }
        );
    }

    #[test]
    fn signalling_an_invalid_pid_is_a_no_op() {
        // pid 0 addresses the caller's process group; it must never be signalled.
        signal(0, nix::sys::signal::Signal::SIGTERM);
        signal(u32::MAX, nix::sys::signal::Signal::SIGTERM);
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    /// Args with every optional field unset, as clap would produce from defaults.
    fn args() -> DispatchArgs {
        DispatchArgs {
            max_sessions: 5,
            repo_root: Some(PathBuf::from("/repo")),
            worktree_root: None,
            state_dir: Some(PathBuf::from("/state")),
            claude_bin: "claude".into(),
            agent: "backend".into(),
            skip_permissions: true,
            run_timeout_min: 120,
            run_timeout_secs: None,
            shutdown_grace_secs: 600,
            github_api_url: "https://api.github.com".into(),
            github_token: None,
        }
    }

    #[test]
    fn worktree_root_defaults_under_the_state_dir() {
        let config = args().into_config();
        assert_eq!(config.worktree_root, PathBuf::from("/state/worktrees"));
    }

    #[test]
    fn an_explicit_worktree_root_wins() {
        let mut a = args();
        a.worktree_root = Some(PathBuf::from("/data/worktrees"));
        assert_eq!(
            a.into_config().worktree_root,
            PathBuf::from("/data/worktrees")
        );
    }

    #[test]
    fn minutes_are_used_unless_seconds_are_given() {
        assert_eq!(
            args().into_config().run_timeout,
            Duration::from_secs(120 * 60)
        );

        // Seconds win, which is what lets a test avoid waiting minutes.
        let mut a = args();
        a.run_timeout_secs = Some(2);
        assert_eq!(a.into_config().run_timeout, Duration::from_secs(2));
    }

    #[test]
    fn an_absurd_timeout_saturates_rather_than_overflowing() {
        let mut a = args();
        a.run_timeout_min = u64::MAX;
        // Would panic in debug on a plain multiply.
        let _ = a.into_config().run_timeout;
    }
}
