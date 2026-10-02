//! Shared harness for the dispatcher tests.
//!
//! Every test gets its own git repository, worktree root, state directory and
//! fake `claude`, so nothing touches the real repository or `~/.local/state`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use conductor::dispatcher::{DispatchConfig, DispatchSink, Dispatcher, PerPullRequestResolver};
use conductor::linear::LinearIssue;
use conductor::registry::{Group, GroupKey, Registry, SystemClock};
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
}

impl Harness {
    /// Builds a bare origin with one commit on `main`, plus a clone to add
    /// worktrees from — the shape the dispatcher expects of a real checkout.
    pub fn new(pool: PgPool) -> Self {
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
        let config = DispatchConfig {
            max_sessions: 5,
            repo_root: repo.clone(),
            worktree_root: worktrees.clone(),
            state_dir: state.clone(),
            claude_bin: fake_claude_path().to_string_lossy().to_string(),
            agent: "backend".into(),
            skip_permissions: true,
            run_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(30),
            // Short so tests observe outside changes quickly.
            rescan_interval: Duration::from_millis(100),
            term_to_kill: Duration::from_millis(200),
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
        );
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
