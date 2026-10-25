//! Harness for the tests that run real tmux and a fake interactive Claude.
//!
//! Every [`TtyHarness`] owns a tmux server of its own (a random `-L` socket) and
//! a loopback hook listener on a random port, so tests run in parallel without
//! seeing each other and never touch the default socket or a developer's tmux.
//! The tmux server is killed on drop, which also runs when a test panics.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor::dispatcher::{Dispatcher, PerPullRequestResolver};
use conductor::hooks;
use conductor::registry::{Group, GroupKey, GroupState};
use conductor::tmux::{SessionRuntime, TmuxRuntime};
use conductor::{EventSink, InboundEvent};
use serde_json::Value;
use sqlx::PgPool;
use tokio::task::JoinHandle;

use super::{shell_quote, Harness};

/// The tmux binary the tests run. Overridable for machines where it is not on
/// `PATH`.
fn tmux_bin() -> String {
    std::env::var("CONDUCTOR_TEST_TMUX").unwrap_or_else(|_| "tmux".into())
}

/// Caps how many real-tmux tests run at once in this process.
///
/// Each test's database pool takes connection permits from sqlx's shared master
/// pool (20 of them), and a dispatcher plus its hook handling opens several
/// connections. Ten such tests at once starve the pool and some fail setup with
/// `PoolTimedOut`, which says nothing about the code under test. Waiting here,
/// before the test does any work, keeps the suite reliable whatever
/// `--test-threads` is. `CONDUCTOR_TEST_TTY_PARALLEL` overrides the default.
///
/// The wait is async on purpose. A test that blocked its thread while waiting
/// would also stop its own pool's background tasks, which return idle
/// connections' permits; the waiting tests would then hold permits the running
/// ones need.
static GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(0);
static GATE_INIT: std::sync::Once = std::sync::Once::new();

struct GateGuard(tokio::sync::SemaphorePermit<'static>);

impl GateGuard {
    async fn acquire() -> GateGuard {
        GATE_INIT.call_once(|| {
            let limit: usize = std::env::var("CONDUCTOR_TEST_TTY_PARALLEL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3);
            GATE.add_permits(limit);
        });
        GateGuard(GATE.acquire().await.expect("gate is never closed"))
    }
}

/// Kills a test's tmux server when dropped, panic or not.
struct TmuxGuard {
    bin: String,
    socket: String,
}

impl Drop for TmuxGuard {
    fn drop(&mut self) {
        let _ = Command::new(&self.bin)
            .args(["-L", &self.socket, "kill-server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // tmux normally unlinks its socket on exit; make sure nothing is left
        // behind in the temp directory after a test that was torn down hard.
        let dir = std::env::var("TMUX_TMPDIR").unwrap_or_else(|_| "/tmp".into());
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("tmux-") {
                    let _ = std::fs::remove_file(entry.path().join(&self.socket));
                }
            }
        }
    }
}

/// One line of the fake Claude's log.
#[derive(Debug, Clone)]
pub struct Entry(pub Value);

impl Entry {
    pub fn event(&self) -> &str {
        self.0["event"].as_str().unwrap_or("")
    }

    pub fn session_id(&self) -> &str {
        self.0["session_id"].as_str().unwrap_or("")
    }

    pub fn pid(&self) -> u64 {
        self.0["pid"].as_u64().unwrap_or(0)
    }

    pub fn ts(&self) -> f64 {
        self.0["ts"].as_f64().unwrap_or(0.0)
    }

    pub fn prompt(&self) -> &str {
        self.0["prompt"].as_str().unwrap_or("")
    }

    pub fn argv(&self) -> Vec<String> {
        self.0["argv"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A client attached to a set's tmux session through a pseudo-terminal.
///
/// `conductor sessions attach` is run under `script`, which gives it a real
/// terminal; whatever is written to [`PtyClient::send`] arrives as keystrokes.
/// Dropping the client hangs the terminal up, which detaches it.
pub struct PtyClient {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
}

impl PtyClient {
    /// Types `text` into the attached terminal.
    pub fn send(&mut self, text: &str) {
        use std::io::Write;
        if let Some(stdin) = &mut self.stdin {
            let _ = stdin.write_all(text.as_bytes());
            let _ = stdin.flush();
        }
    }

    /// Hangs up, detaching the client.
    pub fn detach(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PtyClient {
    fn drop(&mut self) {
        self.detach();
    }
}

/// A dispatcher over real tmux with a fake interactive Claude.
pub struct TtyHarness {
    pub h: Harness,
    pub runtime: Arc<TmuxRuntime>,
    pub socket: String,
    /// The fake Claude's log.
    pub log: PathBuf,
    wrapper: PathBuf,
    knobs: Mutex<BTreeMap<String, String>>,
    /// A replacement for running the fake Claude, once set.
    body_override: Mutex<Option<String>>,
    hook_port: Mutex<Option<u16>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    _guard: TmuxGuard,
    _gate: GateGuard,
}

impl TtyHarness {
    pub async fn new(pool: PgPool) -> Self {
        // Dispatcher logs go to the test's captured output, which libtest shows
        // only when the test fails: exactly when they are wanted.
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .try_init();
        let gate = GateGuard::acquire().await;
        let mut h = Harness::new(pool);
        let socket = format!("test-{}", uuid::Uuid::new_v4().simple());
        let bin = tmux_bin();
        let runtime = Arc::new(TmuxRuntime::new(bin.clone(), socket.clone()));
        let log = h.state.join("fake-claude-tty.jsonl");
        let wrapper = h.state.join("claude-tty");

        // Short enough to observe quickly, long enough for a loaded machine.
        h.config.claude_bin = wrapper.to_string_lossy().into_owned();
        h.config.start_timeout = Duration::from_secs(20);
        h.config.deliver_timeout = Duration::from_secs(20);
        h.config.interrupt_grace = Duration::from_secs(1);
        h.config.run_timeout = Duration::from_secs(60);
        h.config.rescan_interval = Duration::from_millis(100);

        let harness = TtyHarness {
            h,
            runtime,
            socket: socket.clone(),
            log,
            wrapper,
            knobs: Mutex::new(BTreeMap::new()),
            body_override: Mutex::new(None),
            hook_port: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            _guard: TmuxGuard { bin, socket },
            _gate: gate,
        };
        harness.set_knob("FAKE_CLAUDE_TURN_MS", "200");
        harness
    }

    /// Sets an environment variable of the fake Claude, for sessions started
    /// from now on.
    pub fn set_knob(&self, key: &str, value: &str) {
        self.knobs
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        self.write_wrapper();
    }

    /// Makes the "claude" a process that sleeps and never reports `SessionStart`.
    pub fn claude_never_starts(&self) {
        *self.body_override.lock().unwrap() = Some("exec sleep 600".into());
        self.write_wrapper();
    }

    fn write_wrapper(&self) {
        let body_override = self.body_override.lock().unwrap().clone();
        let port = self.hook_port.lock().unwrap().unwrap_or(0);
        let mut script = String::from("#!/bin/sh\n");
        script.push_str(&format!(
            "export FAKE_CLAUDE_LOG={}\nexport FAKE_CLAUDE_HOOK_BIN={}\nexport CONDUCTOR_HOOK_PORT={port}\n",
            shell_quote(&self.log.to_string_lossy()),
            shell_quote(env!("CARGO_BIN_EXE_conductor")),
        ));
        for (k, v) in self.knobs.lock().unwrap().iter() {
            script.push_str(&format!("export {k}={}\n", shell_quote(v)));
        }
        match body_override.as_deref() {
            // Counted, so a test can see how many times a start was attempted.
            Some(body) => script.push_str(&format!(
                "echo '{{\"event\":\"launched\"}}' >> {}\n{body}\n",
                shell_quote(&self.log.to_string_lossy())
            )),
            None => script.push_str(&format!(
                "exec {} \"$@\"\n",
                shell_quote(env!("CARGO_BIN_EXE_fake-claude-tty"))
            )),
        }
        // Written beside and renamed over, so a session starting right now
        // never reads half a script.
        let tmp = self.wrapper.with_extension("tmp");
        super::write_executable(&tmp, &script);
        std::fs::rename(&tmp, &self.wrapper).expect("install wrapper");
    }

    /// Starts a dispatcher and the hook listener. Returns the dispatcher and the
    /// sink to submit events to.
    pub async fn start(&self) -> (Arc<Dispatcher>, conductor::dispatcher::DispatchSink) {
        self.start_with(None).await
    }

    /// Like [`TtyHarness::start`], with automatic closing backed by `source`.
    pub async fn start_with(
        &self,
        source: Option<Arc<dyn conductor::linear_api::IssueSetSource>>,
    ) -> (Arc<Dispatcher>, conductor::dispatcher::DispatchSink) {
        let (dispatcher, sink, rx) = Dispatcher::new(
            self.h.registry.clone(),
            self.h.config.clone(),
            Arc::new(PerPullRequestResolver),
            Arc::clone(&self.h.shutting_down),
            source,
            self.runtime.clone(),
        );

        // A listener per dispatcher on a fresh loopback port: its state belongs
        // to this dispatcher. Sessions started after this get the new port from
        // the wrapper; ones started by an earlier dispatcher keep the old one.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind hook listener");
        let port = listener.local_addr().expect("addr").port();
        *self.hook_port.lock().unwrap() = Some(port);
        self.write_wrapper();
        let app = hooks::router(dispatcher.hook_state());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        self.tasks.lock().unwrap().push(server);

        let ingest = tokio::spawn(Arc::clone(&dispatcher).run_ingest(rx));
        let scheduler = tokio::spawn(Arc::clone(&dispatcher).run_scheduler());
        {
            let mut tasks = self.tasks.lock().unwrap();
            tasks.push(ingest);
            tasks.push(scheduler);
        }
        (dispatcher, sink)
    }

    /// Stops the dispatcher and the hook listener, leaving tmux alone, as when
    /// the process holding them dies.
    pub fn stop_dispatcher(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }

    /// Kills the whole tmux server, as a pod restart does.
    pub fn kill_tmux_server(&self) {
        let _ = Command::new(tmux_bin())
            .args(["-L", &self.socket, "kill-server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    // ── tmux ─────────────────────────────────────────────────────────────────

    /// Runs `tmux -L <socket> <args>` and returns its stdout.
    pub fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new(tmux_bin())
            .args(["-L", &self.socket])
            .args(args)
            .env_remove("TMUX")
            .output()
            .expect("run tmux");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Names of the tmux sessions that exist, sorted.
    pub fn sessions(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tmux(&["list-sessions", "-F", "#{session_name}"])
            .lines()
            .map(str::to_string)
            .collect();
        names.sort();
        names
    }

    /// Types `keys` into a set's pane the way a person would, bypassing
    /// conductor entirely.
    pub fn type_into(&self, slug: &str, keys: &[&str]) {
        let target = format!("={slug}:");
        let mut args = vec!["send-keys", "-t", &target];
        args.extend_from_slice(keys);
        self.tmux(&args);
    }

    /// Number of clients attached to a set's session.
    pub async fn clients(&self, slug: &str) -> usize {
        self.runtime.clients(slug).await.len()
    }

    /// Attaches a client through `conductor sessions attach`, under a terminal.
    pub fn attach(&self, group: &str, read_only: bool) -> PtyClient {
        let mut command = format!(
            "{} sessions attach {group}",
            env!("CARGO_BIN_EXE_conductor")
        );
        if read_only {
            command.push_str(" --read-only");
        }
        let cfg = &self.h.config;
        let mut child = Command::new("script")
            // -q quiet, -f flush, -e propagate the child's exit, -c the command.
            .args(["-qfec", &command, "/dev/null"])
            .env_remove("TMUX")
            .env("TERM", "xterm-256color")
            .env("DATABASE_URL", self.h.database_url())
            .env("CONDUCTOR_REPO_ROOT", &cfg.repo_root)
            .env("CONDUCTOR_WORKTREE_ROOT", &cfg.worktree_root)
            .env("CONDUCTOR_STATE_DIR", &cfg.state_dir)
            .env("CONDUCTOR_CLAUDE_BIN", &cfg.claude_bin)
            .env("CONDUCTOR_AGENT", &cfg.agent)
            .env("CONDUCTOR_TMUX_BIN", tmux_bin())
            .env("CONDUCTOR_TMUX_SOCKET", &self.socket)
            .env("CONDUCTOR_START_TIMEOUT_SECS", "20")
            .env_remove("CONDUCTOR_CLAUDE_MODEL")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("run script");
        let stdin = child.stdin.take();
        PtyClient { child, stdin }
    }

    /// Runs a `conductor sessions` subcommand in-process against real tmux, on a
    /// terminal that records what `attach` would exec.
    pub async fn cli(&self, command: conductor::sessions::SessionsCommand) -> (i32, String) {
        let env = self.h.session_env();
        let terminal = super::FakeTerminal::interactive();
        let mut out: Vec<u8> = Vec::new();
        let code = conductor::sessions::run(
            command,
            &env,
            &self.h.registry,
            self.runtime.as_ref(),
            &terminal,
            &mut out,
        )
        .await
        .expect("sessions command");
        (code, String::from_utf8(out).expect("utf8 output"))
    }

    // ── the fake's log ───────────────────────────────────────────────────────

    /// Every line the fake Claude has logged, in order.
    pub fn entries(&self) -> Vec<Entry> {
        let Ok(body) = std::fs::read_to_string(&self.log) else {
            return Vec::new();
        };
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| Entry(serde_json::from_str(l).expect("fake claude log line")))
            .collect()
    }

    /// Entries of one kind.
    pub fn of(&self, event: &str) -> Vec<Entry> {
        self.entries()
            .into_iter()
            .filter(|e| e.event() == event)
            .collect()
    }

    /// Prompts of completed submissions (`start` entries), in order.
    pub fn prompts(&self) -> Vec<String> {
        self.of("start")
            .iter()
            .map(|e| e.prompt().to_string())
            .collect()
    }

    // ── waiting ──────────────────────────────────────────────────────────────

    /// Waits for the set to be `Ready` having finished `identifier`.
    pub async fn wait_done(&self, key: &GroupKey, identifier: &str) {
        let id = identifier.to_string();
        assert!(
            self.h
                .wait_for_group(key, super::PATIENCE_TTY, |g| {
                    matches!(g.state, GroupState::Ready { .. }) && g.issues.contains(&id)
                })
                .await,
            "{identifier} never finished in {}: {:?}",
            key.slug(),
            self.h.group(key).await.map(|g| g.state)
        );
    }

    /// Waits until the set's state satisfies `pred`.
    pub async fn wait_state(
        &self,
        key: &GroupKey,
        what: &str,
        pred: impl FnMut(&GroupState) -> bool,
    ) {
        assert!(
            self.h.wait_for_state(key, super::PATIENCE_TTY, pred).await,
            "{} never became {what}: {:?}",
            key.slug(),
            self.h.group(key).await.map(|g| g.state)
        );
    }

    /// The group as stored.
    pub async fn group(&self, key: &GroupKey) -> Group {
        self.h.group(key).await.expect("group exists")
    }

    /// Submits a Linear event for `group`.
    pub fn submit(
        &self,
        sink: &conductor::dispatcher::DispatchSink,
        identifier: &str,
        group: &str,
    ) {
        sink.submit(super::linear_event(
            identifier,
            group,
            Some(&format!("d-{identifier}")),
        ));
    }
}

/// The text `prompt_for` pastes for a Linear event.
pub fn expected_prompt(event: &InboundEvent) -> String {
    match event {
        InboundEvent::LinearIssueTodo { issue, .. } => format!(
            "<channel source=\"webhook\" event=\"issue_todo\">{}</channel>",
            serde_json::to_string(issue).expect("issue json")
        ),
        other => panic!("not a Linear todo: {other:?}"),
    }
}

/// Used by the harness for its worktree path assertions.
pub fn same_path(a: &Path, b: &Path) -> bool {
    std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok()
}

/// Ordering shorthand for tests sampling shared counters.
pub const SEQ: Ordering = Ordering::SeqCst;

/// The highest number of turns in flight at once, from the fake's own
/// `start`/`end` entries.
pub fn peak_overlap(entries: &[Entry]) -> usize {
    let mut points: Vec<(f64, i32)> = entries
        .iter()
        .filter_map(|e| match e.event() {
            "start" => Some((e.ts(), 1)),
            "end" => Some((e.ts(), -1)),
            _ => None,
        })
        .collect();
    // Ends sort first at equal timestamps, so touching turns do not overlap.
    points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
    let (mut now, mut peak) = (0i32, 0i32);
    for (_, delta) in points {
        now += delta;
        peak = peak.max(now);
    }
    peak.max(0) as usize
}
