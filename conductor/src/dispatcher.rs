//! Dispatcher: routes accepted events to Claude Code sessions.
//!
//! Runs serially within a session group and in parallel across groups, up to a
//! global cap. Each group works in its own git worktree, so parallel sessions
//! never share a checkout.
//!
//! Replaces the previous arrangement, where every event resumed one hardcoded
//! session in the background — two events close together produced two
//! concurrent resumes of the same conversation.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::github::PrCommentKind;

use crate::closing::{close_group, CloseMode, CloseOutcome};
use crate::hooks::HookState;
use crate::linear_api::{IssueSetSource, LinearApiError};
use crate::registry::{Group, GroupKey, GroupState, QueuedEvent, Registry, RunResult};
use crate::resolve::{fallback_key, GroupResolver};
use crate::tmux::SessionRuntime;
use crate::{EventSink, InboundEvent};

/// How long the scheduler waits before re-scanning when nothing wakes it.
///
/// The CLI mutates the registry from another process, so polling is how those
/// changes take effect without inventing an IPC channel.
const DEFAULT_RESCAN_INTERVAL: Duration = Duration::from_secs(5);

/// How long to wait for `Stop` after interrupting a timed-out turn.
const DEFAULT_INTERRUPT_GRACE: Duration = Duration::from_secs(30);

/// How long a set is left alone after its session failed to start.
const DEFAULT_START_BACKOFF: Duration = Duration::from_secs(300);

/// How often tasks waiting on a hook re-read the registry even without a signal.
const HOOK_POLL: Duration = Duration::from_millis(250);

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

    /// How long a new session may take to report `SessionStart`, in seconds.
    #[arg(long, env = "CONDUCTOR_START_TIMEOUT_SECS", default_value_t = 60)]
    pub start_timeout_secs: u64,

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
            start_timeout: Duration::from_secs(self.start_timeout_secs),
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
    /// How long a new session may take to report `SessionStart`.
    pub start_timeout: Duration,
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

    /// Most tmux sessions (idle ones included) kept alive at once.
    #[arg(long, env = "CONDUCTOR_MAX_LIVE_SESSIONS", default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..))]
    pub max_live_sessions: u16,

    /// How long a pasted prompt may take to report `UserPromptSubmit`, in seconds.
    #[arg(long, env = "CONDUCTOR_DELIVER_TIMEOUT_SECS", default_value_t = 30)]
    pub deliver_timeout_secs: u64,

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
            max_live_sessions: usize::from(self.max_live_sessions),
            start_timeout: env.start_timeout,
            deliver_timeout: Duration::from_secs(self.deliver_timeout_secs),
            interrupt_grace: DEFAULT_INTERRUPT_GRACE,
            start_backoff: DEFAULT_START_BACKOFF,
            rescan_interval: DEFAULT_RESCAN_INTERVAL,
            close_sweep: (self.close_sweep_mins > 0)
                .then(|| Duration::from_secs(self.close_sweep_mins.saturating_mul(60))),
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
    /// Most tmux sessions kept alive at once, idle ones included.
    pub max_live_sessions: usize,
    /// How long a new session may take to report `SessionStart`.
    pub start_timeout: Duration,
    /// How long a pasted prompt may take to report `UserPromptSubmit`.
    pub deliver_timeout: Duration,
    /// How long after an interrupt to wait for `Stop` before killing the session.
    ///
    /// Not a flag: it exists so tests need not wait the production thirty seconds.
    pub interrupt_grace: Duration,
    /// How long a set is left alone after its session failed to start.
    ///
    /// Likewise test-only.
    pub start_backoff: Duration,
    /// How often to re-scan when nothing wakes the scheduler.
    ///
    /// Not exposed as a flag: it exists so tests need not wait the production
    /// five seconds to observe a change made from outside the process.
    pub rescan_interval: Duration,
    /// How often every idle group is checked for a completed set; `None`
    /// disables the sweep.
    pub close_sweep: Option<Duration>,
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

/// Owns the scheduler, the worktrees and the running children.
pub struct Dispatcher {
    registry: Registry,
    config: DispatchConfig,
    resolver: Arc<dyn GroupResolver>,
    runtime: Arc<dyn SessionRuntime>,
    wake: Arc<Notify>,
    /// Signalled by the hook handler; waiters re-read the registry on wake.
    events: Arc<Notify>,
    /// Sets whose session failed to start, and when they may be tried again.
    backoff: StdMutex<HashMap<String, Instant>>,
    /// Sets with a turn-timeout in progress, so it is handled once.
    timing_out: StdMutex<HashSet<String>>,
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
}

impl Dispatcher {
    /// Builds a dispatcher and the sink feeding it.
    pub fn new(
        registry: Registry,
        config: DispatchConfig,
        resolver: Arc<dyn GroupResolver>,
        shutting_down: Arc<AtomicBool>,
        issue_source: Option<Arc<dyn IssueSetSource>>,
        runtime: Arc<dyn SessionRuntime>,
    ) -> (
        Arc<Self>,
        DispatchSink,
        mpsc::UnboundedReceiver<InboundEvent>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let dispatcher = Arc::new(Dispatcher {
            registry,
            config,
            resolver,
            runtime,
            wake: Arc::new(Notify::new()),
            events: Arc::new(Notify::new()),
            backoff: StdMutex::new(HashMap::new()),
            timing_out: StdMutex::new(HashSet::new()),
            repo_lock: Mutex::new(()),
            shutting_down,
            issue_source,
            closing: AtomicBool::new(false),
        });
        (dispatcher, DispatchSink { tx }, rx)
    }

    /// State for the hook listener, sharing this dispatcher's wake-ups.
    pub fn hook_state(&self) -> HookState {
        HookState {
            registry: self.registry.clone(),
            runtime: Arc::clone(&self.runtime),
            wake: Arc::clone(&self.wake),
            events: Arc::clone(&self.events),
        }
    }

    /// Reconciles tmux-model groups with the tmux server.
    pub async fn reconcile_tmux(&self) {
        match self.registry.reconcile_tmux(self.runtime.as_ref()).await {
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

        // Somebody watching the session is a reason to wait, not to ask Linear.
        // The request stays set, so the next scan looks again.
        if !self.runtime.clients(&slug).await.is_empty() {
            tracing::debug!(group = %slug, "set complete but tmux clients are attached; will retry");
            return Ok(());
        }

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

        match close_group(
            &self.registry,
            &self.config.repo_root,
            key,
            CloseMode::Auto,
            Some(self.runtime.as_ref()),
        )
        .await?
        {
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

    // ── scheduling ───────────────────────────────────────────────────────────
    //
    // Everything here is driven by the registry. A scheduling pass looks at the
    // stored states and, for each set with queued work, advances it one step
    // under the row lock (`NoSession → Starting`, `Ready → Delivering`), then
    // hands the slow part — tmux calls and waiting for hooks — to a task. No
    // lock is held across a tmux call or a wait.
    //
    // Concurrency is counted from the registry too: a set is "active" while it
    // is `Starting`, `Delivering` or `Busy`. That is the permit — held from
    // delivery (or a human turn starting) until `Stop`, never by an idle `Ready`
    // session — and, being derived from stored state, it survives restarts and
    // needs no bookkeeping to release.

    /// Advances every set that has work, up to the concurrency limits.
    async fn start_runnable(self: &Arc<Self>) -> Result<(), sqlx::Error> {
        let groups = self.registry.snapshot().await?;

        let is_active = |s: &GroupState| {
            matches!(
                s,
                GroupState::Starting { .. }
                    | GroupState::Delivering { .. }
                    | GroupState::Busy { .. }
                    | GroupState::Running { .. }
            )
        };
        let mut active = groups.iter().filter(|g| is_active(&g.state)).count();
        let mut live = groups
            .iter()
            .filter(|g| g.state.is_tmux() && g.state != GroupState::NoSession)
            .count();

        self.check_timeouts(&groups);

        let mut work: Vec<&Group> = groups.iter().filter(|g| !g.pending.is_empty()).collect();
        work.sort_by_key(|g| g.pending.front().map(|p| p.received_at));

        for g in work {
            if self.shutting_down.load(Ordering::SeqCst) {
                return Ok(());
            }
            if active >= self.config.max_sessions {
                break;
            }
            let key = &g.key;
            match &g.state {
                GroupState::Idle | GroupState::NoSession => {
                    if self.backed_off(&key.slug()) {
                        continue;
                    }
                    if live >= self.config.max_live_sessions {
                        if !self.evict_one(&groups).await {
                            continue;
                        }
                        live -= 1;
                    }
                    let mut txn = self.registry.begin().await?;
                    let started = txn.begin_start(key).await?;
                    txn.commit().await?;
                    if started {
                        active += 1;
                        live += 1;
                        let me = Arc::clone(self);
                        let key = key.clone();
                        tokio::spawn(async move { me.start_session_task(key).await });
                    }
                }
                GroupState::Ready { .. } if g.delivery_blocked.is_none() => {
                    let mut txn = self.registry.begin().await?;
                    let claimed = txn.begin_delivery(key).await?;
                    txn.commit().await?;
                    if let Some(queued) = claimed {
                        active += 1;
                        let me = Arc::clone(self);
                        let key = key.clone();
                        tokio::spawn(async move { me.deliver_task(key, queued).await });
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn backed_off(&self, slug: &str) -> bool {
        let mut map = self.backoff.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(slug) {
            Some(until) if Instant::now() < *until => true,
            Some(_) => {
                map.remove(slug);
                false
            }
            None => false,
        }
    }

    /// Frees one live-session slot by ending the least-recently-active `Ready`
    /// session that nobody is attached to. Never touches `Busy`, `Delivering` or
    /// attached sessions, and skips sets with queued work. Returns whether a
    /// slot was freed.
    async fn evict_one(&self, groups: &[Group]) -> bool {
        let mut candidates: Vec<&Group> = groups
            .iter()
            .filter(|g| matches!(g.state, GroupState::Ready { .. }) && g.pending.is_empty())
            .collect();
        candidates.sort_by_key(|g| g.last_active);

        for g in candidates {
            let slug = g.key.slug();
            if !self.runtime.clients(&slug).await.is_empty() {
                continue;
            }
            let ended = async {
                let mut txn = self.registry.begin().await?;
                // Pinned to the state we looked at: if a turn started meanwhile
                // the session is no longer an eviction candidate.
                let ended = txn
                    .end_session(
                        &g.key,
                        RunResult::Lost,
                        Some(&g.state),
                        chrono::Duration::zero(),
                    )
                    .await?;
                txn.commit().await?;
                Ok::<_, sqlx::Error>(ended)
            }
            .await;
            match ended {
                Ok(true) => {
                    tracing::info!(group = %slug, "evicting idle session to stay under the live-session limit");
                    if let Err(e) = self.runtime.kill(&slug).await {
                        tracing::error!(group = %slug, error = %e, "cannot kill evicted session");
                    }
                    return true;
                }
                Ok(false) => {}
                Err(e) => tracing::error!(group = %slug, error = %e, "eviction failed"),
            }
        }
        false
    }

    /// Waits until `done` accepts the group's stored state, or `timeout` passes.
    async fn wait_for_state(
        &self,
        slug: &str,
        timeout: Duration,
        done: impl FnMut(&GroupState) -> bool,
    ) -> Option<GroupState> {
        wait_for_group_state(&self.registry, slug, timeout, Some(&self.events), done).await
    }

    /// Starts a set's tmux session and waits for it to report in.
    async fn start_session_task(self: Arc<Self>, key: GroupKey) {
        let slug = key.slug();
        match self.start_session(&key).await {
            Ok(()) => tracing::info!(group = %slug, "session ready"),
            Err(reason) => {
                tracing::error!(group = %slug, %reason, "session failed to start; backing off");
                if let Err(e) = self.runtime.kill(&slug).await {
                    tracing::error!(group = %slug, error = %e, "cannot kill failed session");
                }
                let recorded = async {
                    let mut txn = self.registry.begin().await?;
                    let changed = txn.fail_start(&key).await?;
                    txn.commit().await?;
                    Ok::<_, sqlx::Error>(changed)
                }
                .await;
                if let Err(e) = recorded {
                    tracing::error!(group = %slug, error = %e, "cannot record the failed start");
                }
                // The event stays queued; the set is left alone for a while
                // rather than retried in a loop.
                self.backoff
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(slug, Instant::now() + self.config.start_backoff);
            }
        }
        self.wake.notify_one();
    }

    async fn start_session(&self, key: &GroupKey) -> Result<(), String> {
        launch_session(
            &self.registry,
            self.runtime.as_ref(),
            &self.config.start_spec(),
            key,
            Some(&self.repo_lock),
            Some(&self.events),
        )
        .await
    }

    /// Pastes one queued event into a `Ready` session and waits for the turn to
    /// begin.
    async fn deliver_task(self: Arc<Self>, key: GroupKey, queued: QueuedEvent) {
        let slug = key.slug();
        let id = queued.id;
        tracing::info!(group = %slug, issue = ?issue_identifier(&queued.event), "delivering");

        let pasted = self
            .runtime
            .deliver(&slug, &prompt_for(&queued.event))
            .await;
        let failure = match pasted {
            Err(e) => Some(format!("paste failed: {e}")),
            Ok(()) => {
                let confirmed = self
                    .wait_for_state(&slug, self.config.deliver_timeout, |s| {
                        !matches!(s, GroupState::Delivering { event_id, .. } if *event_id == id)
                    })
                    .await;
                confirmed.is_none().then(|| {
                    format!(
                        "no UserPromptSubmit within {}s of pasting",
                        self.config.deliver_timeout.as_secs()
                    )
                })
            }
        };

        if let Some(reason) = failure {
            // The text may be sitting unsent in the input box, so pasting again
            // could submit the task twice. Stop delivering and say so loudly.
            let blocked = async {
                let mut txn = self.registry.begin().await?;
                let b = txn.block_delivery(&key, id, &reason).await?;
                txn.commit().await?;
                Ok::<_, sqlx::Error>(b)
            }
            .await;
            match blocked {
                Ok(true) => tracing::error!(
                    group = %slug, event_id = id, %reason,
                    "delivery not confirmed; delivery to this set is halted until an operator releases it"
                ),
                Ok(false) => {} // The hook arrived in the meantime.
                Err(e) => {
                    tracing::error!(group = %slug, error = %e, "cannot record the blocked delivery")
                }
            }
        }
        self.wake.notify_one();
    }

    /// Starts timeout handling for every turn that has run too long.
    fn check_timeouts(self: &Arc<Self>, groups: &[Group]) {
        let now = chrono::Utc::now();
        for g in groups {
            let GroupState::Busy { since, .. } = &g.state else {
                continue;
            };
            let age = (now - *since).to_std().unwrap_or_default();
            if age <= self.config.run_timeout {
                continue;
            }
            let slug = g.key.slug();
            if !self
                .timing_out
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(slug.clone())
            {
                continue;
            }
            let me = Arc::clone(self);
            let (key, since) = (g.key.clone(), *since);
            tokio::spawn(async move {
                me.time_out_turn(&key, since).await;
                me.timing_out
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&slug);
                me.wake.notify_one();
            });
        }
    }

    /// Interrupts an overlong turn and, failing that, ends the session.
    ///
    /// `TimedOut` is recorded before the interrupt, so the result is the same
    /// whether or not Claude fires `Stop` for an interrupted turn (unverified,
    /// see [`crate::hooks`]); if it does not, the session is killed after the
    /// grace period.
    async fn time_out_turn(&self, key: &GroupKey, since: chrono::DateTime<chrono::Utc>) {
        let slug = key.slug();
        let still_busy = async {
            let mut txn = self.registry.begin().await?;
            let b = txn.time_out_turn(key, since).await?;
            txn.commit().await?;
            Ok::<_, sqlx::Error>(b)
        }
        .await;
        match still_busy {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                tracing::error!(group = %slug, error = %e, "cannot record the turn timeout");
                return;
            }
        }
        tracing::warn!(
            group = %slug,
            timeout_secs = self.config.run_timeout.as_secs(),
            "turn exceeded its timeout; interrupting"
        );
        if let Err(e) = self.runtime.interrupt(&slug).await {
            tracing::error!(group = %slug, error = %e, "cannot interrupt the session");
        }

        let moved_on = self
            .wait_for_state(
                &slug,
                self.config.interrupt_grace,
                |s| !matches!(s, GroupState::Busy { since: s2, .. } if *s2 == since),
            )
            .await;
        if moved_on.is_some() {
            return;
        }

        tracing::warn!(group = %slug, "no Stop after the interrupt; killing the session");
        let current = match self.registry.group_by_slug(&slug).await {
            Ok(Some(g)) => g.state,
            _ => return,
        };
        let ended = async {
            let mut txn = self.registry.begin().await?;
            let e = txn
                .end_session(
                    key,
                    RunResult::TimedOut,
                    Some(&current),
                    chrono::Duration::zero(),
                )
                .await?;
            txn.commit().await?;
            Ok::<_, sqlx::Error>(e)
        }
        .await;
        if let Err(e) = ended {
            tracing::error!(group = %slug, error = %e, "cannot end the timed-out session");
        }
        if let Err(e) = self.runtime.kill(&slug).await {
            tracing::error!(group = %slug, error = %e, "cannot kill the timed-out session");
        }
    }

    /// Stops delivering, lets running turns finish, then ends every session.
    ///
    /// Waits up to the grace period for `Busy` turns to reach `Stop`. Anything
    /// still unfinished is recorded as `Interrupted` and not re-queued:
    /// re-running half-finished work can open a duplicate pull request. Every
    /// tmux session is then killed, idle ones included; after a restart tmux is
    /// gone anyway and every set starts in `NoSession`.
    pub async fn shutdown(self: Arc<Self>) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
        tracing::info!(
            grace_secs = self.config.shutdown_grace.as_secs(),
            "shutting down; waiting for turns to finish"
        );

        let deadline = Instant::now() + self.config.shutdown_grace;
        loop {
            let groups = match self.registry.snapshot().await {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!(error = %e, "cannot read the registry during shutdown");
                    break;
                }
            };
            let mid_turn = groups.iter().any(|g| {
                matches!(
                    g.state,
                    GroupState::Starting { .. }
                        | GroupState::Delivering { .. }
                        | GroupState::Busy { .. }
                )
            });
            if !mid_turn {
                tracing::info!("all turns finished");
                break;
            }
            if Instant::now() >= deadline {
                tracing::warn!("grace period over; interrupting unfinished turns");
                break;
            }
            let nap = HOOK_POLL.min(deadline - Instant::now());
            tokio::select! {
                _ = self.events.notified() => {}
                _ = tokio::time::sleep(nap) => {}
            }
        }

        let groups = self.registry.snapshot().await.unwrap_or_default();
        for g in groups
            .iter()
            .filter(|g| g.state.is_tmux() && g.state != GroupState::NoSession)
        {
            let slug = g.key.slug();
            let ended = async {
                let mut txn = self.registry.begin().await?;
                let e = txn
                    .end_session(
                        &g.key,
                        RunResult::Interrupted,
                        None,
                        chrono::Duration::zero(),
                    )
                    .await?;
                txn.commit().await?;
                Ok::<_, sqlx::Error>(e)
            }
            .await;
            if let Err(e) = ended {
                tracing::error!(group = %slug, error = %e, "cannot record shutdown for the session");
            } else if matches!(
                g.state,
                GroupState::Busy { .. } | GroupState::Delivering { .. }
            ) {
                tracing::warn!(group = %slug, "turn interrupted by shutdown");
            }
            if let Err(e) = self.runtime.kill(&slug).await {
                tracing::error!(group = %slug, error = %e, "cannot kill session during shutdown");
            }
        }
    }
}

/// What starting a set's tmux session needs.
///
/// Shared by the dispatcher and `conductor sessions attach`, so both start a
/// session exactly the same way.
#[derive(Debug, Clone)]
pub struct StartSpec {
    /// Repository the worktrees are added from.
    pub repo_root: PathBuf,
    /// Directory holding one worktree per group.
    pub worktree_root: PathBuf,
    /// Claude binary.
    pub claude_bin: String,
    /// Agent passed to Claude.
    pub agent: String,
    /// Model passed as `--model`; `None` leaves it to Claude Code.
    pub model: Option<String>,
    /// Whether to pass `--dangerously-skip-permissions`.
    pub skip_permissions: bool,
    /// How long to wait for `SessionStart`.
    pub start_timeout: Duration,
}

impl SessionEnv {
    /// The start-path settings, for [`launch_session`].
    pub fn start_spec(&self) -> StartSpec {
        StartSpec {
            repo_root: self.repo_root.clone(),
            worktree_root: self.worktree_root.clone(),
            claude_bin: self.claude_bin.clone(),
            agent: self.agent.clone(),
            model: self.model.clone(),
            skip_permissions: self.skip_permissions,
            start_timeout: self.start_timeout,
        }
    }
}

impl DispatchConfig {
    /// The start-path settings, for [`launch_session`].
    pub fn start_spec(&self) -> StartSpec {
        StartSpec {
            repo_root: self.repo_root.clone(),
            worktree_root: self.worktree_root.clone(),
            claude_bin: self.claude_bin.clone(),
            agent: self.agent.clone(),
            model: self.model.clone(),
            skip_permissions: self.skip_permissions,
            start_timeout: self.start_timeout,
        }
    }
}

/// Waits until `done` accepts the group's stored state, or `timeout` passes.
///
/// The registry is the source of truth: every wake re-reads it. `events`, when
/// given, makes a hook wake the wait immediately; the poll covers a signal that
/// fired before listening began, and callers in another process (the CLI) that
/// have no signal at all. `None` means the group is gone or the wait timed out.
pub async fn wait_for_group_state(
    registry: &Registry,
    slug: &str,
    timeout: Duration,
    events: Option<&Notify>,
    mut done: impl FnMut(&GroupState) -> bool,
) -> Option<GroupState> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match registry.group_by_slug(slug).await {
            Ok(Some(g)) if done(&g.state) => return Some(g.state),
            Ok(Some(_)) => {}
            Ok(None) => return None,
            Err(e) => {
                tracing::error!(group = %slug, error = %e, "cannot read group while waiting")
            }
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        let nap = HOOK_POLL.min(deadline - now);
        match events {
            Some(events) => {
                tokio::select! {
                    _ = events.notified() => {}
                    _ = tokio::time::sleep(nap) => {}
                }
            }
            None => tokio::time::sleep(nap).await,
        }
    }
}

/// Starts a set's tmux session and waits for it to report `SessionStart`.
///
/// The caller has already moved the group to `Starting` (see
/// `RegistryTxn::begin_start`). On error nothing is cleaned up: the caller
/// kills the session and records the failed start.
pub async fn launch_session(
    registry: &Registry,
    runtime: &dyn SessionRuntime,
    spec: &StartSpec,
    key: &GroupKey,
    repo_lock: Option<&Mutex<()>>,
    events: Option<&Notify>,
) -> Result<(), String> {
    let slug = key.slug();
    let base_ref = registry
        .worktree_ref(key)
        .await
        .map_err(|e| e.to_string())?;
    let worktree = {
        // Repository setup only; never held while waiting on Claude. Serialised
        // in-process by the dispatcher's lock; the CLI has no equivalent and
        // relies on git's own locking.
        let _repo = match repo_lock {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        ensure_worktree(
            &spec.repo_root,
            &spec.worktree_root,
            &slug,
            base_ref.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())?
    };

    let (session_id, resume) = {
        let mut txn = registry.begin().await.map_err(|e| e.to_string())?;
        txn.set_worktree(key, &worktree)
            .await
            .map_err(|e| e.to_string())?;
        // Recorded now as well as after each turn: routing a pull request
        // comment back to this group needs every branch it has worked on.
        if let Some(branch) = current_branch(&spec.worktree_root, &slug).await {
            txn.record_branch(key, &branch)
                .await
                .map_err(|e| e.to_string())?;
        }
        let r = txn
            .session_for_dispatch(key)
            .await
            .map_err(|e| e.to_string())?;
        txn.commit().await.map_err(|e| e.to_string())?;
        r
    };

    let mut argv = vec![spec.claude_bin.clone()];
    argv.push(if resume { "--resume" } else { "--session-id" }.into());
    argv.push(session_id.to_string());
    argv.push(format!("--agent={}", spec.agent));
    // Precedence of `--model` against a `model:` in the agent's frontmatter
    // (Claude Code 2.1.285, `--agent=backend`) is NOT verified: it could not
    // be probed without a logged-in Claude in the dev sandbox. `backend.md`
    // has no `model:`, so `--model` applies today either way. Before adding
    // one, run `claude --agent=<a> --model sonnet --print ...` and read
    // `.message.model` in the transcript (see the NEX-146 PR).
    //
    // Passed on resume as well as on a fresh start, so a model change takes
    // effect for a set's next turn rather than only for a brand-new session.
    if let Some(model) = spec.model.as_deref() {
        argv.push("--model".into());
        argv.push(model.to_string());
    }
    if spec.skip_permissions {
        argv.push("--dangerously-skip-permissions".into());
    }
    // Each worktree needs its own build cache; a shared target directory
    // would serialise parallel sessions on cargo's lock.
    let env = vec![(
        "CARGO_TARGET_DIR".to_string(),
        worktree.join("target").to_string_lossy().into_owned(),
    )];

    tracing::info!(group = %slug, resume, "starting session");
    runtime
        .start(&slug, &worktree, &argv, &env)
        .await
        .map_err(|e| e.to_string())?;

    let reached = wait_for_group_state(registry, &slug, spec.start_timeout, events, |s| {
        !matches!(s, GroupState::Starting { .. })
    })
    .await;
    start_outcome(reached, spec.start_timeout)
}

/// Whether a session counts as started, given the first state seen once it left
/// `Starting`.
///
/// Anything past `Starting` is success: the scheduler may already have claimed
/// the group for delivery (`Delivering`) or a human may already have typed
/// (`Busy`) by the time this looks. Only a session that has ended is a failure,
/// and `None` means nothing arrived in time.
fn start_outcome(reached: Option<GroupState>, timeout: Duration) -> Result<(), String> {
    match reached {
        Some(GroupState::NoSession | GroupState::Idle) => {
            Err("session ended while starting".into())
        }
        Some(_) => Ok(()),
        None => Err(format!("no SessionStart within {}s", timeout.as_secs())),
    }
}

/// The branch currently checked out in a group's worktree.
async fn current_branch(worktree_root: &Path, slug: &str) -> Option<String> {
    let out = run_git(&worktree_root.join(slug), &["branch", "--show-current"])
        .await
        .ok()?;
    let name = out.trim().to_string();
    (!name.is_empty()).then_some(name)
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
}

#[cfg(test)]
mod config_tests {
    use super::*;

    /// Args with every optional field unset, as clap would produce from defaults.
    fn args() -> DispatchArgs {
        DispatchArgs {
            max_sessions: 5,
            max_live_sessions: 10,
            deliver_timeout_secs: 30,
            env: SessionEnvArgs {
                repo_root: Some(PathBuf::from("/repo")),
                worktree_root: None,
                state_dir: Some(PathBuf::from("/state")),
                claude_bin: "claude".into(),
                agent: "backend".into(),
                model: None,
                start_timeout_secs: 60,
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
    fn a_session_that_moved_on_before_we_looked_still_counts_as_started() {
        let now = chrono::Utc::now();
        let t = Duration::from_secs(60);
        for state in [
            GroupState::Ready {
                pane_pid: 1,
                since: now,
            },
            // The scheduler can claim the group the instant it is Ready.
            GroupState::Delivering {
                since: now,
                event_id: 1,
            },
            GroupState::Busy {
                since: now,
                delivered_event: None,
            },
        ] {
            assert_eq!(start_outcome(Some(state.clone()), t), Ok(()), "{state:?}");
        }
        assert!(start_outcome(Some(GroupState::NoSession), t).is_err());
        assert!(start_outcome(None, t).unwrap_err().contains("60s"));
    }

    #[test]
    fn tmux_limits_default_from_the_args() {
        let c = args().into_config();
        assert_eq!(c.max_live_sessions, 10);
        assert_eq!(c.start_timeout, Duration::from_secs(60));
        assert_eq!(c.deliver_timeout, Duration::from_secs(30));
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
