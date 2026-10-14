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

use crate::closing::{close_group, CloseMode, CloseOutcome};
use crate::linear_api::{IssueSetSource, LinearApiError};
use crate::registry::{GroupKey, QueuedEvent, Registry, RunOutcome, RunResult};
use crate::resolve::{fallback_key, GroupResolver};
use crate::tmux::SessionRuntime;
use crate::{EventSink, InboundEvent};
/// Re-exported so callers can name a signal without depending on `nix`.
pub use nix::sys::signal::Signal;

/// How long the scheduler waits before re-scanning when nothing wakes it.
///
/// The CLI mutates the registry from another process, so polling is how those
/// changes take effect without inventing an IPC channel.
const DEFAULT_RESCAN_INTERVAL: Duration = Duration::from_secs(5);

/// Grace between `SIGTERM` and `SIGKILL` for a run being stopped.
const DEFAULT_TERM_TO_KILL: Duration = Duration::from_secs(30);

/// How often `sessions attach` polls a group that is still running.
pub const DEFAULT_ATTACH_POLL: Duration = Duration::from_secs(2);

/// Settings shared by `conductor serve` and `conductor sessions`.
///
/// Both surfaces need to know where the repository, the worktrees and the state
/// live, and which binary to run. Defining them once means the two cannot
/// disagree about a default and then operate on different directories.
#[derive(Debug, Clone, clap::Args)]
pub struct SessionEnvArgs {
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

    /// Model passed to Claude as `--model`: an alias such as `sonnet`, or a full
    /// model ID. Unset or empty leaves the choice to Claude Code.
    #[arg(long, env = "CONDUCTOR_CLAUDE_MODEL", value_parser = parse_model)]
    pub model: Option<String>,

    /// Pass `--dangerously-skip-permissions` to Claude.
    ///
    /// Governs dispatched runs and `sessions attach` alike. The interactive
    /// path originally omitted it on the reasoning that an attached operator
    /// could answer permission prompts — which does not hold over
    /// `kubectl exec`, where the prompts simply block the session.
    #[arg(long, env = "CONDUCTOR_SKIP_PERMISSIONS", default_value_t = true, action = clap::ArgAction::Set)]
    pub skip_permissions: bool,
}

impl SessionEnvArgs {
    /// Fills in the defaults that depend on other settings.
    pub fn resolve(self) -> SessionEnv {
        let state_dir = self.state_dir.unwrap_or_else(default_state_dir);
        SessionEnv {
            repo_root: self
                .repo_root
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into())),
            worktree_root: self
                .worktree_root
                .unwrap_or_else(|| state_dir.join("worktrees")),
            state_dir,
            claude_bin: self.claude_bin,
            agent: self.agent,
            model: self.model.filter(|m| !m.is_empty()),
            skip_permissions: self.skip_permissions,
            kill_grace: DEFAULT_TERM_TO_KILL,
            poll_interval: DEFAULT_ATTACH_POLL,
        }
    }
}

/// Resolved form of [`SessionEnvArgs`].
#[derive(Debug, Clone)]
pub struct SessionEnv {
    /// Repository the worktrees are added from.
    pub repo_root: PathBuf,
    /// Directory holding one worktree per group.
    pub worktree_root: PathBuf,
    /// Directory holding run logs.
    pub state_dir: PathBuf,
    /// Claude binary to execute.
    pub claude_bin: String,
    /// Agent passed through to Claude.
    pub agent: String,
    /// Model passed as `--model`; `None` leaves it to Claude Code.
    pub model: Option<String>,
    /// Whether to pass `--dangerously-skip-permissions`.
    pub skip_permissions: bool,
    /// Grace between `SIGTERM` and `SIGKILL` when the CLI stops a process.
    ///
    /// Not exposed as a flag: it exists so tests need not wait the production
    /// thirty seconds to watch the kill path through.
    pub kill_grace: Duration,
    /// How often `sessions attach` re-checks a group it is waiting for.
    ///
    /// Likewise test-only.
    pub poll_interval: Duration,
}

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

    #[command(flatten)]
    pub env: SessionEnvArgs,

    /// How long a run may take, in minutes.
    #[arg(long, env = "CONDUCTOR_RUN_TIMEOUT_MIN", default_value_t = 120)]
    pub run_timeout_min: u64,

    /// Run timeout in seconds. Takes precedence, so tests need not wait minutes.
    #[arg(long, env = "CONDUCTOR_RUN_TIMEOUT_SECS")]
    pub run_timeout_secs: Option<u64>,

    /// How long shutdown waits for runs to finish, in seconds.
    #[arg(long, env = "CONDUCTOR_SHUTDOWN_GRACE_SECS", default_value_t = 600)]
    pub shutdown_grace_secs: u64,

    /// How often to re-check every idle group for a completed set, in minutes.
    /// `0` disables the sweep.
    #[arg(long, env = "CONDUCTOR_CLOSE_SWEEP_MINS", default_value_t = 30)]
    pub close_sweep_mins: u64,

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
        let env = self.env.resolve();
        DispatchConfig {
            max_sessions: usize::from(self.max_sessions),
            repo_root: env.repo_root,
            worktree_root: env.worktree_root,
            state_dir: env.state_dir,
            claude_bin: env.claude_bin,
            agent: env.agent,
            model: env.model,
            skip_permissions: env.skip_permissions,
            // Seconds win when given, so a test can use a timeout far under a minute.
            run_timeout: Duration::from_secs(
                self.run_timeout_secs
                    .unwrap_or_else(|| self.run_timeout_min.saturating_mul(60)),
            ),
            shutdown_grace: Duration::from_secs(self.shutdown_grace_secs),
            rescan_interval: DEFAULT_RESCAN_INTERVAL,
            close_sweep: (self.close_sweep_mins > 0)
                .then(|| Duration::from_secs(self.close_sweep_mins.saturating_mul(60))),
            term_to_kill: DEFAULT_TERM_TO_KILL,
        }
    }
}

/// Validates a model name against `^[a-z0-9][a-z0-9.-]*(\[1m\])?$`.
///
/// Empty is accepted and means "no `--model` flag". Hand-rolled to avoid a
/// regex dependency for one pattern.
pub fn parse_model(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    let base = value.strip_suffix("[1m]").unwrap_or(value);
    let mut chars = base.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
    if valid {
        Ok(value.to_string())
    } else {
        Err(format!(
            "invalid CONDUCTOR_CLAUDE_MODEL {value:?}: must match ^[a-z0-9][a-z0-9.-]*(\\[1m\\])?$"
        ))
    }
}

/// Adds `--model` to a Claude command line when a model is configured.
///
/// Precedence against an agent definition's `model:` frontmatter: see the
/// comment at the call site in `Dispatcher::execute`.
pub fn push_model_arg(command: &mut Command, model: Option<&str>) {
    if let Some(model) = model {
        command.arg("--model").arg(model);
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
    /// Model passed as `--model`; `None` leaves it to Claude Code.
    pub model: Option<String>,
    /// Whether to pass `--dangerously-skip-permissions`.
    pub skip_permissions: bool,
    /// How long a run may take before it is stopped.
    pub run_timeout: Duration,
    /// How long shutdown waits for runs to finish.
    pub shutdown_grace: Duration,
    /// How often to re-scan when nothing wakes the scheduler.
    ///
    /// Not exposed as a flag: it exists so tests need not wait the production
    /// five seconds to observe a change made from outside the process.
    pub rescan_interval: Duration,
    /// How often every idle group is checked for a completed set; `None`
    /// disables the sweep.
    pub close_sweep: Option<Duration>,
    /// Grace between `SIGTERM` and `SIGKILL` when stopping a run.
    ///
    /// Likewise test-only, so the kill path can be exercised without a 30s wait.
    pub term_to_kill: Duration,
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
    /// Lists a set's issues for the completeness check. `None` disables
    /// automatic closing.
    issue_source: Option<Arc<dyn IssueSetSource>>,
    /// Set while a completeness pass runs, so passes never overlap.
    closing: AtomicBool,
    /// Serialises setup against the shared git repository.
    ///
    /// `git fetch` and `git worktree add` both write `.git/config` and take
    /// repository-wide locks, so running them concurrently against one checkout
    /// fails rather than queueing: measured at 17 failures in 40 attempts with
    /// five at a time, reporting `could not lock config file .git/config`. With
    /// the default cap of five groups that is the normal case, not an edge one.
    /// Only setup is serialised — the runs themselves stay parallel.
    repo_lock: Mutex<()>,
    /// tmux runtime reconciled on each scheduler pass; unset until the
    /// dispatcher drives tmux sessions.
    runtime: std::sync::OnceLock<Arc<dyn SessionRuntime>>,
}

impl Dispatcher {
    /// Builds a dispatcher and the sink feeding it.
    pub fn new(
        registry: Registry,
        config: DispatchConfig,
        resolver: Arc<dyn GroupResolver>,
        shutting_down: Arc<AtomicBool>,
        issue_source: Option<Arc<dyn IssueSetSource>>,
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
            repo_lock: Mutex::new(()),
            shutting_down,
            issue_source,
            closing: AtomicBool::new(false),
            runtime: std::sync::OnceLock::new(),
        });
        (dispatcher, DispatchSink { tx }, rx)
    }

    /// Supplies the tmux runtime to reconcile against. Only the first call has
    /// an effect.
    pub fn set_runtime(&self, runtime: Arc<dyn SessionRuntime>) {
        let _ = self.runtime.set(runtime);
    }

    /// Reconciles tmux-model groups with the tmux server, if one is configured.
    pub async fn reconcile_tmux(&self) {
        let Some(runtime) = self.runtime.get() else {
            return;
        };
        match self.registry.reconcile_tmux(runtime.as_ref()).await {
            Ok((0, 0)) => {}
            Ok((reset, killed)) => {
                tracing::warn!(reset, killed, "tmux reconciliation changed state")
            }
            Err(e) => tracing::error!(error = %e, "tmux reconciliation failed"),
        }
    }

    /// Persists an accepted event and wakes the scheduler.
    ///
    /// Resolution happens before the lock is taken, so a slow lookup never
    /// holds up other writers.
    async fn ingest(&self, event: InboundEvent) {
        if let InboundEvent::LinearIssueClosed { .. } = &event {
            self.ingest_closed(&event).await;
            return;
        }

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

    /// Flags the closed issue's group for a completeness check.
    ///
    /// Never queued as work and never starts `claude`. A set that never ran has
    /// no group, so there is nothing to do.
    async fn ingest_closed(&self, event: &InboundEvent) {
        if self.issue_source.is_none() {
            return;
        }
        let key = crate::registry::group_key_for(event);
        let delivery = delivery_id(event).map(str::to_string);

        let mut txn = match self.registry.begin().await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "cannot reach the registry; close event dropped");
                return;
            }
        };
        let result = async {
            if !txn.mark_delivery_seen(delivery.as_deref()).await? {
                tracing::info!(delivery_id = ?delivery, "duplicate delivery ignored");
                return Ok(false);
            }
            txn.request_close(&key).await
        }
        .await;
        match result {
            Ok(requested) => {
                if let Err(e) = txn.commit().await {
                    tracing::error!(error = %e, "failed to persist close request");
                    return;
                }
                if requested {
                    tracing::info!(group = %key.slug(), "close requested; set will be checked");
                    self.wake.notify_one();
                }
            }
            Err(e) => tracing::error!(error = %e, "failed to record close request"),
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

        if self.issue_source.is_none() {
            tracing::info!(
                "LINEAR_API_KEY not set; automatic closing of finished sets is disabled"
            );
        }
        // `None` makes the first pass sweep, catching sets finished while down.
        let mut last_sweep: Option<Instant> = None;

        loop {
            if self.shutting_down.load(Ordering::SeqCst) {
                tracing::info!("scheduler stopping; no new runs will start");
                return;
            }

            self.reconcile_tmux().await;

            if let Err(e) = self.start_runnable().await {
                tracing::error!(error = %e, "scheduling pass failed");
            }

            let sweep = self
                .config
                .close_sweep
                .is_some_and(|every| last_sweep.is_none_or(|t| t.elapsed() >= every));
            if sweep {
                last_sweep = Some(Instant::now());
            }
            self.spawn_close_pass(sweep);

            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(self.config.rescan_interval) => {}
            }
        }
    }

    /// Runs a completeness pass in the background, unless one is still going.
    ///
    /// Detached because each check calls Linear, which must not hold up
    /// scheduling.
    fn spawn_close_pass(self: &Arc<Self>, sweep: bool) {
        if self.issue_source.is_none()
            || self
                .closing
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.close_pass(sweep).await;
            me.closing.store(false, Ordering::SeqCst);
        });
    }

    /// Checks groups for a completed set, one at a time.
    ///
    /// `sweep` widens the scope from groups a webhook flagged to every
    /// eligible group. One group failing never stops the others.
    pub async fn close_pass(&self, sweep: bool) {
        let keys = match self.registry.close_candidates(!sweep).await {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(error = %e, "cannot list groups to check for closing");
                return;
            }
        };
        for key in keys {
            if self.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            if let Err(e) = self.check_and_close(&key).await {
                tracing::error!(group = %key.slug(), error = %e, "close check failed");
            }
        }
    }

    /// Closes the group if Linear says every issue in its set is complete.
    ///
    /// Linear is queried first and without any lock; local state is re-verified
    /// under the lock by [`close_group`]. Any uncertainty means not closing, and
    /// the request is withdrawn so the next sweep, not the next 5s scan, retries.
    async fn check_and_close(&self, key: &GroupKey) -> Result<(), sqlx::Error> {
        let Some(source) = &self.issue_source else {
            return Ok(());
        };
        let slug = key.slug();

        let issues = match source.issues_in_set(key).await {
            Ok(issues) => issues,
            Err(LinearApiError::NotASet) => {
                return self.registry.clear_close_request(key).await;
            }
            Err(e) => {
                tracing::warn!(group = %slug, error = %e, "cannot read the set from Linear; not closing");
                return self.registry.clear_close_request(key).await;
            }
        };

        if issues.is_empty() {
            tracing::warn!(
                group = %slug,
                "Linear returned no issues for this set (label renamed or deleted?); not closing"
            );
            return self.registry.clear_close_request(key).await;
        }
        if let Some(open) = issues
            .iter()
            .find(|i| !matches!(i.state_type.as_str(), "completed" | "canceled"))
        {
            tracing::debug!(group = %slug, open = %open.identifier, "set still has open issues");
            return self.registry.clear_close_request(key).await;
        }

        match close_group(&self.registry, &self.config.repo_root, key, CloseMode::Auto).await? {
            CloseOutcome::Closed { .. } | CloseOutcome::Gone => {}
            CloseOutcome::Busy => {
                tracing::info!(group = %slug, reason = "busy", "set complete but group is busy; will retry when idle");
            }
            CloseOutcome::HasPending => {
                tracing::info!(group = %slug, reason = "pending events", "set complete but work is queued; not closing");
            }
            CloseOutcome::WorktreeDirty(reason) => {
                tracing::warn!(group = %slug, reason = %reason, "worktree could not be removed; not closing");
                self.registry.block_close(key, &reason).await?;
            }
        }
        Ok(())
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
        let worktree = {
            // Scoped so the lock is released long before the child is waited
            // on: it covers repository setup only, never the run.
            let _repo = self.repo_lock.lock().await;
            self.ensure_worktree(&slug, base_ref.as_deref()).await?
        };

        let mut txn = self.registry.begin().await?;
        txn.set_worktree(key, &worktree).await?;
        // Recorded now as well as on completion: routing a pull request comment
        // back to this group needs every branch it has worked on, and the branch
        // it started from would otherwise be lost the moment the agent switches.
        if let Some(branch) = self.current_branch(&slug).await {
            txn.record_branch(key, &branch).await?;
        }
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
        // Precedence of `--model` against a `model:` in the agent's frontmatter
        // (Claude Code 2.1.285, `--agent=backend`) is NOT verified: it could not
        // be probed without a logged-in Claude in the dev sandbox. `backend.md`
        // has no `model:`, so `--model` applies today either way. Before adding
        // one, run `claude --agent=<a> --model sonnet --print ...` and read
        // `.message.model` in the transcript (see the NEX-146 PR).
        push_model_arg(&mut command, self.config.model.as_deref());
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
                terminate(pid, child, self.config.term_to_kill).await;
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
                model: self.config.model.clone(),
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
        ensure_worktree(
            &self.config.repo_root,
            &self.config.worktree_root,
            slug,
            base_ref,
        )
        .await
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
            signal_process(handle.pid, nix::sys::signal::Signal::SIGTERM);
        }
        drop(running);

        tokio::time::sleep(self.config.term_to_kill).await;
        for (slug, handle) in self.running.lock().await.iter() {
            tracing::warn!(group = %slug, pid = handle.pid, "run did not stop; killing it");
            signal_process(handle.pid, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

/// Prepares the git worktree a group works in, creating it on first use.
///
/// Shared with `conductor sessions attach`, which has to be able to take over a
/// group that has never run and therefore has no checkout yet.
///
/// Never deletes anything: a path that exists but is not a worktree is reported
/// rather than cleared, since whatever is there may be real work.
pub async fn ensure_worktree(
    repo_root: &Path,
    worktree_root: &Path,
    slug: &str,
    base_ref: Option<&str>,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    // The slug is validated on the way in, so it cannot escape this root.
    let path = worktree_root.join(slug);

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

    std::fs::create_dir_all(worktree_root)?;

    // A group answering comments on an existing pull request works on that
    // pull request's branch; starting a fresh branch from main would have the
    // session editing code the review was not about.
    if let Some(base) = base_ref {
        run_git(repo_root, &["fetch", "origin", base]).await?;
        let path_str = path.to_string_lossy().to_string();
        run_git(repo_root, &["worktree", "add", &path_str, base]).await?;
        tracing::info!(worktree = %path.display(), branch = %base, "worktree ready on pull request branch");
        return Ok(path);
    }

    run_git(repo_root, &["fetch", "origin", "main"]).await?;

    let branch = format!("agent/{slug}");
    let branch_exists = run_git(
        repo_root,
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
    run_git(repo_root, &args).await?;
    tracing::info!(worktree = %path.display(), branch = %branch, "worktree ready");
    Ok(path)
}

/// Sends a signal, ignoring a process that has already gone.
pub fn signal_process(pid: u32, sig: nix::sys::signal::Signal) {
    // `kill` reads 0 and negative pids as "a process group", not "a process",
    // so they must never reach it.
    if pid == 0 || pid > i32::MAX as u32 {
        return;
    }
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), sig);
}

/// `SIGTERM`, then `SIGKILL` if the child is still there.
async fn terminate(pid: u32, child: &mut Child, grace: Duration) {
    signal_process(pid, nix::sys::signal::Signal::SIGTERM);
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        signal_process(pid, nix::sys::signal::Signal::SIGKILL);
        let _ = child.wait().await;
    }
}

/// Runs git, returning stdout or the captured error.
pub(crate) async fn run_git(
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
        InboundEvent::LinearIssueClosed { .. } => "issue_closed",
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
        InboundEvent::LinearIssueClosed { issue, .. } => serde_json::to_string(issue),
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
        InboundEvent::LinearIssueClosed { issue, .. } => Some(issue.identifier.clone()),
        InboundEvent::GithubPrComment { .. } => None,
    }
}

/// The provider's delivery identifier, used for idempotency.
fn delivery_id(event: &InboundEvent) -> Option<&str> {
    match event {
        InboundEvent::LinearIssueTodo { delivery_id, .. }
        | InboundEvent::LinearIssueClosed { delivery_id, .. }
        | InboundEvent::GithubPrComment { delivery_id, .. } => delivery_id.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[derive(Parser)]
    struct Probe {
        #[command(flatten)]
        env: SessionEnvArgs,
    }

    fn model_from(args: &[&str]) -> Result<Option<String>, clap::Error> {
        let argv = std::iter::once("probe").chain(args.iter().copied());
        Probe::try_parse_from(argv).map(|p| p.env.resolve().model)
    }

    #[test]
    fn model_flag_is_parsed_and_unset_or_empty_means_none() {
        assert_eq!(
            model_from(&["--model", "sonnet"]).unwrap().as_deref(),
            Some("sonnet")
        );
        assert_eq!(
            model_from(&["--model", "claude-sonnet-5-5"])
                .unwrap()
                .as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(
            model_from(&["--model", "opus[1m]"]).unwrap().as_deref(),
            Some("opus[1m]")
        );
        assert_eq!(model_from(&[]).unwrap(), None);
        assert_eq!(model_from(&["--model", ""]).unwrap(), None);
    }

    #[test]
    fn invalid_models_are_rejected() {
        for bad in [
            "Sonnet",
            "-x",
            ".x",
            "a b",
            "a;b",
            "sonnet[2m]",
            "[1m]",
            "a_b",
            "--x",
        ] {
            assert!(parse_model(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(model_from(&["--model=Sonnet"]).is_err());
    }

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
        signal_process(0, nix::sys::signal::Signal::SIGTERM);
        signal_process(u32::MAX, nix::sys::signal::Signal::SIGTERM);
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    /// Args with every optional field unset, as clap would produce from defaults.
    fn args() -> DispatchArgs {
        DispatchArgs {
            max_sessions: 5,
            env: SessionEnvArgs {
                repo_root: Some(PathBuf::from("/repo")),
                worktree_root: None,
                state_dir: Some(PathBuf::from("/state")),
                claude_bin: "claude".into(),
                agent: "backend".into(),
                model: None,
                skip_permissions: true,
            },
            run_timeout_min: 120,
            run_timeout_secs: None,
            shutdown_grace_secs: 600,
            github_api_url: "https://api.github.com".into(),
            close_sweep_mins: 30,
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
        a.env.worktree_root = Some(PathBuf::from("/data/worktrees"));
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
