//! Shared harness for the dispatcher tests.
//!
//! Every test gets its own git repository, worktree root, state directory and
//! fake `claude`, so nothing touches the real repository or `~/.local/state`.

#![allow(dead_code)]

pub mod tty;

/// How long a real-tmux test may wait for work it expects to happen. A ceiling
/// for contention, not an expected duration: a wrong assertion still fails at once.
pub const PATIENCE_TTY: std::time::Duration = std::time::Duration::from_secs(45);

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor::dispatcher::{
    DispatchConfig, DispatchSink, Dispatcher, PerPullRequestResolver, SessionEnv,
};
use conductor::hooks::{self, HookPayload, HookState};
use conductor::linear::LinearIssue;
use conductor::registry::{Group, GroupKey, GroupState, Registry, SystemClock};
use conductor::sessions::{self, SessionsCommand, Terminal};
use conductor::tmux::{SessionRuntime, TmuxError};
use conductor::InboundEvent;
use serde::Deserialize;
use sqlx::PgPool;
use tempfile::TempDir;

/// One line from the fake claude's log.
#[derive(Debug, Deserialize)]
pub struct FakeRun {
    pub event: String,
    pub pid: u32,
    pub ts: f64,
    pub cwd: String,
    pub argv: Vec<String>,
    pub env_cargo_target_dir: Option<String>,
}

impl FakeRun {
    /// The session id this invocation used, and whether it resumed.
    pub fn session(&self) -> Option<(String, bool)> {
        let position = self
            .argv
            .iter()
            .position(|a| a == "--session-id" || a == "--resume")?;
        let resumed = self.argv[position] == "--resume";
        Some((self.argv.get(position + 1)?.clone(), resumed))
    }
}

impl FakeRun {
    /// The value following `--model`, if the flag was passed.
    pub fn model(&self) -> Option<&str> {
        let position = self.argv.iter().position(|a| a == "--model")?;
        self.argv.get(position + 1).map(String::as_str)
    }
}

/// Runs git, panicking with its stderr so a broken fixture is obvious.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

// ── fake tmux + Claude ───────────────────────────────────────────────────────

struct FakeSession {
    session_id: String,
    cwd: PathBuf,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    clients: Vec<String>,
    interrupt: Arc<tokio::sync::Notify>,
    /// Set when the session is killed, so a turn in flight stops reporting.
    dead: Arc<AtomicBool>,
}

/// A `SessionRuntime` that plays both tmux and the Claude inside it.
///
/// `start` answers with a `SessionStart` hook, `deliver` with a
/// `UserPromptSubmit`, a pause and then a `Stop`, all through the real hook
/// handler, so the registry sees exactly what it would from a live pod. Turns
/// are logged in the fake-claude JSON format, so tests measure order and overlap
/// from observed behaviour.
pub struct FakeTmux {
    /// The hook state holds this runtime, so this is a reference cycle until
    /// [`FakeTmux::detach_hooks`] is called. The cycle would keep the test's
    /// database pool alive after the test, and sqlx only gives a test pool's
    /// connection permits back to its parent once the pool is dropped.
    hook: Mutex<Option<HookState>>,
    log: PathBuf,
    turn_ms: AtomicU64,
    cmd: Mutex<Option<String>>,
    /// Ignore `interrupt`, standing in for a Claude that never reacts to Escape.
    pub ignore_interrupt: AtomicBool,
    /// When false, `start` never produces `SessionStart`.
    pub send_session_start: AtomicBool,
    /// When false, `deliver` never produces `UserPromptSubmit`.
    pub send_prompt_submit: AtomicBool,
    /// How many Enters to drop before one takes effect, standing in for Claude
    /// Code's TUI losing an Enter that arrives with the paste it follows. The
    /// text stays in `pending`, so a later `submit` still starts the turn —
    /// which is exactly the production behaviour this models.
    pub swallow_enters: AtomicUsize,
    /// Pasted text not yet submitted, by slug.
    pending: Mutex<HashMap<String, String>>,
    /// When false, a finished turn produces no `Stop`.
    pub send_stop: AtomicBool,
    sessions: Mutex<HashMap<String, FakeSession>>,
    /// Everything pasted, as `(slug, text)`.
    pub delivered: Mutex<Vec<(String, String)>>,
    /// Slugs killed, in order.
    pub killed: Mutex<Vec<String>>,
    /// Slugs whose clients were detached, in order.
    pub detached: Mutex<Vec<String>>,
    /// Slugs interrupted, in order.
    pub interrupted: Mutex<Vec<String>>,
    /// Slugs started, in order.
    pub started: Mutex<Vec<String>>,
}

impl FakeTmux {
    pub fn new(log: PathBuf) -> Arc<Self> {
        Arc::new(FakeTmux {
            hook: Mutex::new(None),
            log,
            turn_ms: AtomicU64::new(50),
            cmd: Mutex::new(None),
            ignore_interrupt: AtomicBool::new(false),
            send_session_start: AtomicBool::new(true),
            send_prompt_submit: AtomicBool::new(true),
            swallow_enters: AtomicUsize::new(0),
            pending: Mutex::new(HashMap::new()),
            send_stop: AtomicBool::new(true),
            sessions: Mutex::new(HashMap::new()),
            delivered: Mutex::new(Vec::new()),
            killed: Mutex::new(Vec::new()),
            detached: Mutex::new(Vec::new()),
            interrupted: Mutex::new(Vec::new()),
            started: Mutex::new(Vec::new()),
        })
    }

    pub fn attach_hooks(&self, state: HookState) {
        *self.hook.lock().unwrap() = Some(state);
    }

    /// Breaks the cycle between this runtime and its hook state.
    pub fn detach_hooks(&self) {
        *self.hook.lock().unwrap() = None;
    }

    pub fn set_turn(&self, ms: u64, cmd: Option<&str>) {
        self.turn_ms.store(ms, Ordering::SeqCst);
        *self.cmd.lock().unwrap() = cmd.map(str::to_string);
    }

    /// Pretends `clients` are attached to the session.
    pub fn set_clients(&self, slug: &str, clients: &[&str]) {
        if let Some(s) = self.sessions.lock().unwrap().get_mut(slug) {
            s.clients = clients.iter().map(|c| c.to_string()).collect();
        }
    }

    /// Whether the fake still holds this session.
    pub fn has(&self, slug: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(slug)
    }

    /// Drops the session without telling anyone, as a crash would.
    pub fn crash(&self, slug: &str) {
        if let Some(s) = self.sessions.lock().unwrap().remove(slug) {
            s.dead.store(true, Ordering::SeqCst);
        }
    }

    /// Sends a hook for this session, as the real `conductor hook` would.
    pub async fn hook(&self, slug: &str, name: &str) {
        let session_id = {
            let sessions = self.sessions.lock().unwrap();
            match sessions.get(slug) {
                Some(s) => s.session_id.clone(),
                None => return,
            }
        };
        self.hook_for(&session_id, name).await;
    }

    async fn hook_for(&self, session_id: &str, name: &str) {
        let Some(state) = self.hook.lock().unwrap().clone() else {
            return;
        };
        let payload = HookPayload {
            hook_event_name: name.into(),
            session_id: session_id.into(),
            ..Default::default()
        };
        hooks::process(&state, &payload).await;
    }
}

/// A terminal that records what `attach` would exec instead of doing it.
pub struct FakeTerminal {
    interactive: bool,
    /// Every command line `attach` handed over to tmux.
    pub execs: Mutex<Vec<Vec<String>>>,
}

impl FakeTerminal {
    pub fn interactive() -> Self {
        FakeTerminal {
            interactive: true,
            execs: Mutex::new(Vec::new()),
        }
    }

    pub fn piped() -> Self {
        FakeTerminal {
            interactive: false,
            execs: Mutex::new(Vec::new()),
        }
    }
}

impl Terminal for FakeTerminal {
    fn is_interactive(&self) -> bool {
        self.interactive
    }

    fn exec(&self, command: &[String]) -> i32 {
        self.execs.lock().unwrap().push(command.to_vec());
        0
    }
}

/// Appends one record in the fake-claude log format.
fn record_turn(log: &Path, event: &str, cwd: &Path, argv: &[String], env: &[(String, String)]) {
    use std::io::Write;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let line = serde_json::json!({
        "event": event,
        "pid": 0,
        "ts": ts,
        "cwd": cwd,
        "argv": argv.iter().skip(1).collect::<Vec<_>>(),
        "env_cargo_target_dir": env.iter()
            .find(|(k, _)| k == "CARGO_TARGET_DIR").map(|(_, v)| v),
    });
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .expect("open fake log");
    writeln!(f, "{line}").expect("write fake log");
}

#[async_trait::async_trait]
impl SessionRuntime for FakeTmux {
    async fn start(
        &self,
        slug: &str,
        worktree: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<(), TmuxError> {
        let session_id = argv
            .iter()
            .position(|a| a == "--session-id" || a == "--resume")
            .and_then(|i| argv.get(i + 1))
            .cloned()
            .unwrap_or_default();
        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.contains_key(slug) {
                return Err(TmuxError::Failed {
                    command: "new-session".into(),
                    status: "1".into(),
                    stderr: "duplicate session".into(),
                });
            }
            sessions.insert(
                slug.to_string(),
                FakeSession {
                    session_id: session_id.clone(),
                    cwd: worktree.to_path_buf(),
                    argv: argv.to_vec(),
                    env: env.to_vec(),
                    clients: vec![],
                    interrupt: Arc::new(tokio::sync::Notify::new()),
                    dead: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        self.started.lock().unwrap().push(slug.to_string());
        if self.send_session_start.load(Ordering::SeqCst) {
            let me = self.hook.lock().unwrap().clone();
            if let Some(state) = me {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    let payload = HookPayload {
                        hook_event_name: "SessionStart".into(),
                        session_id,
                        ..Default::default()
                    };
                    hooks::process(&state, &payload).await;
                });
            }
        }
        Ok(())
    }

    async fn exists(&self, slug: &str) -> bool {
        self.has(slug)
    }

    async fn pane_pid(&self, slug: &str) -> Option<u32> {
        self.has(slug).then_some(1)
    }

    async fn clients(&self, slug: &str) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap()
            .get(slug)
            .map(|s| s.clients.clone())
            .unwrap_or_default()
    }

    async fn list_sessions(&self) -> Result<Vec<String>, TmuxError> {
        Ok(self.sessions.lock().unwrap().keys().cloned().collect())
    }

    async fn deliver(&self, slug: &str, text: &str) -> Result<(), TmuxError> {
        // Only existence matters here; `submit` reads what it needs.
        if !self.sessions.lock().unwrap().contains_key(slug) {
            return Err(TmuxError::Failed {
                command: "load-buffer".into(),
                status: "1".into(),
                stderr: "no such session".into(),
            });
        }
        self.delivered
            .lock()
            .unwrap()
            .push((slug.to_string(), text.to_string()));
        self.pending
            .lock()
            .unwrap()
            .insert(slug.to_string(), text.to_string());
        // The Enter that follows the paste, which may be lost.
        self.submit(slug).await
    }

    async fn submit(&self, slug: &str) -> Result<(), TmuxError> {
        let (session_id, interrupt, dead, cwd, argv, env) = {
            let sessions = self.sessions.lock().unwrap();
            let Some(s) = sessions.get(slug) else {
                return Err(TmuxError::Failed {
                    command: "send-keys".into(),
                    status: "1".into(),
                    stderr: "no such session".into(),
                });
            };
            (
                s.session_id.clone(),
                Arc::clone(&s.interrupt),
                Arc::clone(&s.dead),
                s.cwd.clone(),
                s.argv.clone(),
                s.env.clone(),
            )
        };
        if !self.send_prompt_submit.load(Ordering::SeqCst) {
            return Ok(());
        }
        // A swallowed Enter leaves the text pending, as the real TUI does.
        if self
            .swallow_enters
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Ok(());
        }
        // Nothing in the composer: Enter on an empty prompt does nothing.
        let Some(text) = self.pending.lock().unwrap().remove(slug) else {
            return Ok(());
        };
        let Some(state) = self.hook.lock().unwrap().clone() else {
            return Ok(());
        };

        let turn = Duration::from_millis(self.turn_ms.load(Ordering::SeqCst));
        let cmd = self.cmd.lock().unwrap().clone();
        let ignore = self.ignore_interrupt.load(Ordering::SeqCst);
        let send_stop = self.send_stop.load(Ordering::SeqCst);
        let log = self.log.clone();

        // The pasted prompt is logged as the last argument, where the old
        // `--print` run carried it, so tests read it from the same place.
        let mut argv = argv;
        argv.push(text.to_string());
        // Logged before the hook so a test waiting on the state sees the start.
        record_turn(&log, "start", &cwd, &argv, &env);

        tokio::spawn(async move {
            let payload = |name: &str| HookPayload {
                hook_event_name: name.into(),
                session_id: session_id.clone(),
                ..Default::default()
            };
            hooks::process(&state, &payload("UserPromptSubmit")).await;
            if ignore {
                tokio::time::sleep(turn).await;
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(turn) => {}
                    _ = interrupt.notified() => {}
                }
            }
            if let Some(cmd) = cmd {
                let _ = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .current_dir(&cwd)
                    .output()
                    .await;
            }
            // A killed session's Claude is gone; it reports nothing further.
            if dead.load(Ordering::SeqCst) {
                return;
            }
            record_turn(&log, "end", &cwd, &argv, &env);
            if send_stop {
                hooks::process(&state, &payload("Stop")).await;
            }
        });
        Ok(())
    }

    async fn interrupt(&self, slug: &str) -> Result<(), TmuxError> {
        self.interrupted.lock().unwrap().push(slug.to_string());
        if self.ignore_interrupt.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(s) = self.sessions.lock().unwrap().get(slug) {
            s.interrupt.notify_one();
        }
        Ok(())
    }

    async fn kill(&self, slug: &str) -> Result<(), TmuxError> {
        self.killed.lock().unwrap().push(slug.to_string());
        if let Some(s) = self.sessions.lock().unwrap().remove(slug) {
            s.dead.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn detach_clients(&self, slug: &str) -> Result<(), TmuxError> {
        self.detached.lock().unwrap().push(slug.to_string());
        if let Some(s) = self.sessions.lock().unwrap().get_mut(slug) {
            s.clients.clear();
        }
        Ok(())
    }

    fn attach_command(&self, slug: &str, read_only: bool) -> Vec<String> {
        let mut cmd = vec!["fake-tmux".to_string(), "attach".to_string()];
        if read_only {
            cmd.push("-r".into());
        }
        cmd.push(slug.to_string());
        cmd
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.fake.detach_hooks();
    }
}

/// A dispatcher wired to throwaway paths.
pub struct Harness {
    _tmp: TempDir,
    /// Kept so tests can run raw SQL to simulate outside changes.
    pub pool: PgPool,
    pub repo: PathBuf,
    pub origin: PathBuf,
    pub worktrees: PathBuf,
    pub state: PathBuf,
    pub log: PathBuf,
    pub config: DispatchConfig,
    pub registry: Registry,
    pub shutting_down: Arc<AtomicBool>,
    /// The fake tmux/Claude the dispatcher drives.
    pub fake: Arc<FakeTmux>,
}

impl Harness {
    /// Builds a bare origin with one commit on `main`, plus a clone to add
    /// worktrees from — the shape the dispatcher expects of a real checkout.
    pub fn new(pool: PgPool) -> Self {
        // Dispatcher logs go to the test's captured output, which libtest shows
        // only when the test fails: exactly when they are wanted.
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .try_init();
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let origin = root.join("origin.git");
        let repo = root.join("repo");
        let worktrees = root.join("worktrees");
        let state = root.join("state");
        std::fs::create_dir_all(&repo).expect("repo dir");
        std::fs::create_dir_all(&state).expect("state dir");

        git(
            &root,
            &["init", "--bare", "--initial-branch=main", "origin.git"],
        );
        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "seed\n").expect("seed file");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "seed"]);
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().expect("utf8")],
        );
        git(&repo, &["push", "-u", "origin", "main"]);

        let log = state.join("fake-claude.jsonl");
        let state_log = log.clone();
        let config = DispatchConfig {
            max_sessions: 5,
            repo_root: repo.clone(),
            worktree_root: worktrees.clone(),
            state_dir: state.clone(),
            claude_bin: fake_claude_path().to_string_lossy().to_string(),
            agent: "backend".into(),
            model: Some("sonnet".into()),
            skip_permissions: true,
            run_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(30),
            // Short so tests observe outside changes quickly.
            rescan_interval: Duration::from_millis(100),
            close_sweep: None,
            max_live_sessions: 10,
            start_timeout: Duration::from_secs(10),
            deliver_timeout: Duration::from_secs(10),
            interrupt_grace: Duration::from_millis(300),
            start_backoff: Duration::from_millis(500),
        };

        Harness {
            _tmp: tmp,
            repo,
            origin,
            worktrees,
            state,
            log,
            registry: Registry::new(pool.clone(), Arc::new(SystemClock), 7),
            pool,
            config,
            shutting_down: Arc::new(AtomicBool::new(false)),
            fake: FakeTmux::new(state_log),
        }
    }

    /// Points the dispatcher at a wrapper script carrying this test's fake
    /// settings.
    ///
    /// Baked into a per-test script rather than exported into this process:
    /// `#[sqlx::test]` runs tests in parallel threads, so shared environment
    /// variables would race and tests would see each other's settings.
    pub fn with_fake(
        mut self,
        sleep_ms: u64,
        exit: i32,
        cmd: Option<&str>,
        ignore_term: bool,
    ) -> Self {
        let wrapper = self.state.join("claude-wrapper");
        let fixture = fake_claude_path();
        let cmd_line = match cmd {
            Some(c) => format!("export FAKE_CLAUDE_CMD={}\n", shell_quote(c)),
            None => String::new(),
        };
        let script = format!(
            "#!/usr/bin/env bash\n\
             export FAKE_CLAUDE_LOG={log}\n\
             export FAKE_CLAUDE_SLEEP_MS={sleep_ms}\n\
             export FAKE_CLAUDE_EXIT={exit}\n\
             export FAKE_CLAUDE_IGNORE_TERM={ignore}\n\
             {cmd_line}\
             exec {fixture} \"$@\"\n",
            log = shell_quote(&self.log.to_string_lossy()),
            ignore = if ignore_term { 1 } else { 0 },
            fixture = shell_quote(&fixture.to_string_lossy()),
        );
        std::fs::write(&wrapper, script).expect("write wrapper");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
                .expect("chmod wrapper");
        }
        self.config.claude_bin = wrapper.to_string_lossy().to_string();
        // Dispatched turns run on the fake tmux; the wrapper above only serves
        // `sessions attach`, which still launches claude itself.
        self.fake.set_turn(sleep_ms, cmd);
        self.fake
            .ignore_interrupt
            .store(ignore_term, Ordering::SeqCst);
        self
    }

    /// Overrides the run timeout for this harness.
    pub fn with_run_timeout(mut self, timeout: Duration) -> Self {
        self.config.run_timeout = timeout;
        self
    }

    /// Overrides the global concurrency cap.
    pub fn with_max_sessions(mut self, max: usize) -> Self {
        self.config.max_sessions = max;
        self
    }

    /// Overrides how long shutdown waits before stopping runs.
    pub fn with_shutdown_grace(mut self, grace: Duration) -> Self {
        self.config.shutdown_grace = grace;
        self
    }

    /// Starts the dispatcher and returns it with its sink.
    pub fn start(&self) -> (Arc<Dispatcher>, DispatchSink) {
        let (dispatcher, sink, rx) = Dispatcher::new(
            self.registry.clone(),
            self.config.clone(),
            Arc::new(PerPullRequestResolver),
            Arc::clone(&self.shutting_down),
            None,
            self.fake.clone(),
        );
        self.fake.attach_hooks(dispatcher.hook_state());
        tokio::spawn(Arc::clone(&dispatcher).run_ingest(rx));
        tokio::spawn(Arc::clone(&dispatcher).run_scheduler());
        (dispatcher, sink)
    }

    /// Like [`Harness::start`], with automatic closing backed by `source`.
    pub fn start_with_source(
        &self,
        source: Arc<dyn conductor::linear_api::IssueSetSource>,
    ) -> (Arc<Dispatcher>, DispatchSink) {
        let (dispatcher, sink, rx) = Dispatcher::new(
            self.registry.clone(),
            self.config.clone(),
            Arc::new(PerPullRequestResolver),
            Arc::clone(&self.shutting_down),
            Some(source),
            self.fake.clone(),
        );
        self.fake.attach_hooks(dispatcher.hook_state());
        tokio::spawn(Arc::clone(&dispatcher).run_ingest(rx));
        tokio::spawn(Arc::clone(&dispatcher).run_scheduler());
        (dispatcher, sink)
    }

    /// Every run the fake has logged.
    pub fn runs(&self) -> Vec<FakeRun> {
        let Ok(body) = std::fs::read_to_string(&self.log) else {
            return Vec::new();
        };
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("fake claude log line"))
            .collect()
    }

    /// Only the start records, in the order they were written.
    pub fn starts(&self) -> Vec<FakeRun> {
        self.runs()
            .into_iter()
            .filter(|r| r.event == "start")
            .collect()
    }

    /// The group as currently stored, if it exists.
    pub async fn group(&self, key: &GroupKey) -> Option<Group> {
        let slug = key.slug();
        self.registry
            .snapshot()
            .await
            .expect("snapshot")
            .into_iter()
            .find(|g| g.key.slug() == slug)
    }

    /// Waits until `predicate` holds, or gives up. Returns whether it held.
    pub async fn wait_for(
        &self,
        within: Duration,
        mut predicate: impl FnMut(&Harness) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        while tokio::time::Instant::now() < deadline {
            if predicate(self) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        predicate(self)
    }

    /// Waits until the fake has logged `n` completed runs.
    pub async fn wait_for_runs(&self, n: usize, within: Duration) -> bool {
        self.wait_for(within, |h| {
            h.runs().iter().filter(|r| r.event == "end").count() >= n
        })
        .await
    }

    /// Waits until the stored group satisfies `predicate`.
    ///
    /// Async throughout, because `#[sqlx::test]` runs each test on a
    /// current-thread runtime where blocking on a registry read inside a
    /// synchronous closure would panic.
    pub async fn wait_for_group(
        &self,
        key: &GroupKey,
        within: Duration,
        mut predicate: impl FnMut(&Group) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if let Some(group) = self.group(key).await {
                if predicate(&group) {
                    return true;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the group reaches a state matching `predicate`.
    pub async fn wait_for_state(
        &self,
        key: &GroupKey,
        within: Duration,
        mut predicate: impl FnMut(&GroupState) -> bool,
    ) -> bool {
        self.wait_for_group(key, within, |g| predicate(&g.state))
            .await
    }

    // ── `conductor sessions` ────────────────────────────────────────────────

    /// The CLI's view of this harness's paths and binary.
    pub fn session_env(&self) -> SessionEnv {
        SessionEnv {
            repo_root: self.repo.clone(),
            worktree_root: self.worktrees.clone(),
            state_dir: self.state.clone(),
            claude_bin: self.config.claude_bin.clone(),
            agent: self.config.agent.clone(),
            model: self.config.model.clone(),
            skip_permissions: self.config.skip_permissions,
            start_timeout: self.config.start_timeout,
        }
    }

    /// Runs a subcommand on an interactive terminal, returning its exit code
    /// and everything it printed.
    pub async fn sessions(&self, command: SessionsCommand) -> (i32, String) {
        self.sessions_on(command, &FakeTerminal::interactive())
            .await
    }

    /// Like [`Harness::sessions`], on a chosen terminal.
    pub async fn sessions_on(
        &self,
        command: SessionsCommand,
        terminal: &FakeTerminal,
    ) -> (i32, String) {
        let env = self.session_env();
        let mut out: Vec<u8> = Vec::new();
        let code = sessions::run(
            command,
            &env,
            &self.registry,
            self.fake.as_ref(),
            terminal,
            &mut out,
        )
        .await
        .expect("sessions command");
        (code, String::from_utf8(out).expect("utf8 output"))
    }

    /// Queues an event without dispatching it.
    pub async fn enqueue(&self, key: &GroupKey, event: &InboundEvent) {
        let mut txn = self.registry.begin().await.expect("begin");
        txn.session_for_dispatch(key).await.expect("create group");
        txn.enqueue(key, event).await.expect("enqueue");
        txn.commit().await.expect("commit");
    }

    /// Forces a group's recorded state, standing in for an outside change.
    pub async fn set_state(&self, key: &GroupKey, state: &GroupState) {
        sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
            .bind(key.slug())
            .bind(sqlx::types::Json(state))
            .execute(&self.pool)
            .await
            .expect("set state");
    }

    /// The group's current state, or `None` when the group is gone.
    pub async fn state_of(&self, key: &GroupKey) -> Option<GroupState> {
        self.group(key).await.map(|g| g.state)
    }

    /// `DATABASE_URL` for this test's own database, for a child process.
    ///
    /// `#[sqlx::test]` hands each test a freshly created database and only a
    /// pool onto it, so the URL has to be rebuilt: the base URL's credentials
    /// with this test's database name swapped in. Without this a spawned
    /// `conductor` would connect to the base database and see none of the
    /// fixtures.
    pub fn database_url(&self) -> String {
        let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for these tests");
        let database = self
            .pool
            .connect_options()
            .get_database()
            .expect("test database name")
            .to_string();

        // Keep scheme, credentials, host and any query string; replace the path.
        let (scheme, rest) = base
            .split_once("://")
            .unwrap_or_else(|| panic!("DATABASE_URL has no scheme: {base}"));
        let (authority, query) = match rest.find('?') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let authority = authority.split('/').next().unwrap_or(authority);
        format!("{scheme}://{authority}/{database}{query}")
    }

    /// Spawns the real `conductor` binary against this test's fixtures.
    ///
    /// Used where the behaviour under test belongs to a process rather than a
    /// function — signal handling, which cannot be observed in-process without
    /// the test runner itself catching the signal.
    pub fn spawn_cli(&self, args: &[&str]) -> std::process::Child {
        let env = self.session_env();
        std::process::Command::new(env!("CARGO_BIN_EXE_conductor"))
            .args(args)
            .env("DATABASE_URL", self.database_url())
            .env("CONDUCTOR_REPO_ROOT", &self.repo)
            .env("CONDUCTOR_WORKTREE_ROOT", &self.worktrees)
            .env("CONDUCTOR_STATE_DIR", &self.state)
            .env("CONDUCTOR_CLAUDE_BIN", &env.claude_bin)
            .env("CONDUCTOR_AGENT", &env.agent)
            .env("CONDUCTOR_CLAUDE_MODEL", env.model.as_deref().unwrap_or(""))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn conductor")
    }
}

/// Single-quotes a value for safe inclusion in the generated script.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Path to the fake claude fixture.
pub fn fake_claude_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude")
}

/// A Linear event in the given session group.
pub fn linear_event(identifier: &str, group: &str, delivery: Option<&str>) -> InboundEvent {
    InboundEvent::LinearIssueTodo {
        issue: LinearIssue {
            identifier: identifier.into(),
            title: "t".into(),
            description: "d".into(),
            labels: vec![format!("session:{group}")],
            session_labels: vec![group.into()],
        },
        delivery_id: delivery.map(str::to_string),
    }
}

/// The highest number of runs overlapping at any instant, swept from the log.
///
/// Measured from the fake's own timestamps rather than dispatcher internals, so
/// the assertion is about observed behaviour.
pub fn max_overlap(runs: &[FakeRun]) -> usize {
    let mut points: Vec<(f64, i32)> = Vec::new();
    for run in runs {
        points.push((run.ts, if run.event == "start" { 1 } else { -1 }));
    }
    // Ends sort before starts at equal timestamps, so touching runs do not
    // count as overlapping.
    points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));

    let mut current = 0i32;
    let mut peak = 0i32;
    for (_, delta) in points {
        current += delta;
        peak = peak.max(current);
    }
    peak.max(0) as usize
}
