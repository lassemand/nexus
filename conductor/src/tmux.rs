//! The tmux runtime: one long-lived tmux session per group.
//!
//! Every call goes through `tokio::process::Command::arg` — never `sh -c` — so
//! group slugs, paths and prompt text are never parsed by a shell. Every call
//! also has a timeout, so a hung tmux server cannot hang the scheduler.
//!
//! All conductor sessions live on one tmux server selected with `-L <socket>`,
//! which is what lets `kubectl exec … tmux -L conductor attach` join them.
//!
//! Session targets use tmux's exact-match form (`=slug`), because a plain name
//! would prefix-match: `has-session -t foo` succeeds when only `foo-bar` exists.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Default tmux binary.
pub const DEFAULT_TMUX_BIN: &str = "tmux";
/// Default tmux socket name, passed as `-L`.
pub const DEFAULT_TMUX_SOCKET: &str = "conductor";
/// How long any single tmux invocation may take.
pub const TMUX_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a tmux operation failed.
#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    /// The tmux binary could not be started.
    #[error("cannot run tmux: {0}")]
    Spawn(#[source] std::io::Error),
    /// tmux did not finish within [`TMUX_TIMEOUT`].
    #[error("tmux {0} timed out")]
    Timeout(String),
    /// tmux ran and reported failure.
    #[error("tmux {command} failed ({status}): {stderr}")]
    Failed {
        command: String,
        status: String,
        stderr: String,
    },
    /// Talking to the child's pipes failed.
    #[error("tmux io error: {0}")]
    Io(#[source] std::io::Error),
}

/// Operations on per-group interactive sessions.
///
/// A trait so the dispatcher and CLI can be tested against a fake.
#[async_trait]
pub trait SessionRuntime: Send + Sync + 'static {
    /// Starts a detached session running `argv` in `worktree`. Fails if one
    /// with this name already exists.
    async fn start(
        &self,
        slug: &str,
        worktree: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<(), TmuxError>;

    /// Whether a session with this name exists.
    async fn exists(&self, slug: &str) -> bool;

    /// PID of the process in the session's pane.
    async fn pane_pid(&self, slug: &str) -> Option<u32>;

    /// Names of the clients attached to the session; empty when none or absent.
    async fn clients(&self, slug: &str) -> Vec<String>;

    /// Names of all sessions on the conductor tmux server.
    async fn list_sessions(&self) -> Result<Vec<String>, TmuxError>;

    /// Pastes `text` as one bracketed paste, then submits it with Enter.
    ///
    /// The Enter can be lost: Claude Code's TUI does not always act on one that
    /// arrives in the same instant as the paste it follows, which left tasks
    /// sitting unsent in the composer until delivery timed out. The dispatcher
    /// therefore re-sends it via [`SessionRuntime::submit`] until the
    /// `UserPromptSubmit` hook confirms the turn started.
    async fn deliver(&self, slug: &str, text: &str) -> Result<(), TmuxError>;

    /// Sends Enter alone, submitting whatever is in the composer.
    ///
    /// Separate from [`SessionRuntime::deliver`] so a submission can be retried
    /// without re-pasting the text. Re-pasting could run a task twice;
    /// re-submitting cannot — it either submits the pending text once or, if
    /// the composer is already empty, does nothing a TUI will act on.
    async fn submit(&self, slug: &str) -> Result<(), TmuxError>;

    /// Sends Escape, which interrupts a running turn.
    async fn interrupt(&self, slug: &str) -> Result<(), TmuxError>;

    /// Kills the session. Succeeds if it does not exist.
    async fn kill(&self, slug: &str) -> Result<(), TmuxError>;

    /// Detaches every client from the session, leaving it running.
    async fn detach_clients(&self, slug: &str) -> Result<(), TmuxError>;

    /// The command line (program first) that attaches a terminal to the
    /// session; `read_only` adds `-r`. Run by `conductor sessions attach`, which
    /// replaces itself with it so the terminal belongs to tmux.
    fn attach_command(&self, slug: &str, read_only: bool) -> Vec<String>;
}

/// [`SessionRuntime`] backed by the `tmux` CLI.
#[derive(Debug, Clone)]
pub struct TmuxRuntime {
    bin: String,
    socket: String,
}

/// Captured result of one tmux invocation.
struct Output {
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

impl TmuxRuntime {
    /// A runtime using `bin` and the `-L` socket name `socket`.
    pub fn new(bin: impl Into<String>, socket: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            socket: socket.into(),
        }
    }

    /// Reads `CONDUCTOR_TMUX_BIN` and `CONDUCTOR_TMUX_SOCKET`.
    pub fn from_env() -> Self {
        let get = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Self::new(
            get("CONDUCTOR_TMUX_BIN", DEFAULT_TMUX_BIN),
            get("CONDUCTOR_TMUX_SOCKET", DEFAULT_TMUX_SOCKET),
        )
    }

    /// Runs `tmux -L <socket> <args>`, optionally feeding `stdin`.
    async fn run(&self, args: &[String], stdin: Option<&str>) -> Result<Output, TmuxError> {
        let label = args.first().cloned().unwrap_or_default();
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-L")
            .arg(&self.socket)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A timed-out call must not leave a tmux client behind.
            .kill_on_drop(true);

        let fut = async {
            let mut child = cmd.spawn().map_err(TmuxError::Spawn)?;
            if let Some(text) = stdin {
                if let Some(mut pipe) = child.stdin.take() {
                    pipe.write_all(text.as_bytes())
                        .await
                        .map_err(TmuxError::Io)?;
                    // Dropped here, closing stdin so `load-buffer -` sees EOF.
                }
            }
            child.wait_with_output().await.map_err(TmuxError::Io)
        };
        let out = tokio::time::timeout(TMUX_TIMEOUT, fut)
            .await
            .map_err(|_| TmuxError::Timeout(label))??;
        Ok(Output {
            success: out.status.success(),
            status: out.status.to_string(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }

    /// Like [`Self::run`], but a non-zero exit is an error.
    async fn run_ok(&self, args: &[String], stdin: Option<&str>) -> Result<Output, TmuxError> {
        let out = self.run(args, stdin).await?;
        if out.success {
            Ok(out)
        } else {
            Err(TmuxError::Failed {
                command: args.first().cloned().unwrap_or_default(),
                status: out.status,
                stderr: out.stderr,
            })
        }
    }
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Exact-match session target.
fn session(slug: &str) -> String {
    format!("={slug}")
}

/// Exact-match target for the session's active pane.
fn pane(slug: &str) -> String {
    format!("={slug}:")
}

fn buffer(slug: &str) -> String {
    format!("conductor-{slug}")
}

#[async_trait]
impl SessionRuntime for TmuxRuntime {
    async fn start(
        &self,
        slug: &str,
        worktree: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<(), TmuxError> {
        let mut a = args(&["new-session", "-d", "-s", slug, "-c"]);
        a.push(worktree.to_string_lossy().into_owned());
        a.extend(args(&["-x", "220", "-y", "50"]));
        // `-e` needs tmux 3.2 or newer.
        for (k, v) in env {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        // With several arguments tmux execs them directly, without a shell.
        a.extend(argv.iter().cloned());
        self.run_ok(&a, None).await.map(|_| ())
    }

    async fn exists(&self, slug: &str) -> bool {
        match self
            .run(&args(&["has-session", "-t", &session(slug)]), None)
            .await
        {
            Ok(out) => out.success,
            Err(e) => {
                tracing::warn!(slug, error = %e, "tmux has-session failed");
                false
            }
        }
    }

    async fn pane_pid(&self, slug: &str) -> Option<u32> {
        let out = self
            .run_ok(
                &args(&["display", "-p", "-t", &pane(slug), "#{pane_pid}"]),
                None,
            )
            .await
            .ok()?;
        out.stdout.trim().parse().ok()
    }

    async fn clients(&self, slug: &str) -> Vec<String> {
        match self
            .run_ok(
                &args(&["list-clients", "-t", &session(slug), "-F", "#{client_name}"]),
                None,
            )
            .await
        {
            Ok(out) => out
                .stdout
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn list_sessions(&self) -> Result<Vec<String>, TmuxError> {
        let out = self
            .run(&args(&["list-sessions", "-F", "#{session_name}"]), None)
            .await?;
        if out.success {
            return Ok(out
                .stdout
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect());
        }
        // No server means no sessions; anything else is a real failure.
        if out.stderr.contains("no server running") || out.stderr.contains("error connecting") {
            return Ok(Vec::new());
        }
        Err(TmuxError::Failed {
            command: "list-sessions".into(),
            status: out.status,
            stderr: out.stderr,
        })
    }

    async fn deliver(&self, slug: &str, text: &str) -> Result<(), TmuxError> {
        let buf = buffer(slug);
        // Text goes over stdin, never as an argument, so it is neither
        // length-limited nor interpreted as a tmux key name.
        self.run_ok(&args(&["load-buffer", "-b", &buf, "-"]), Some(text))
            .await?;
        // -p: bracketed paste, so newlines do not submit early. -d: drop the buffer.
        self.run_ok(
            &args(&["paste-buffer", "-p", "-d", "-b", &buf, "-t", &pane(slug)]),
            None,
        )
        .await?;
        self.submit(slug).await
    }

    async fn submit(&self, slug: &str) -> Result<(), TmuxError> {
        self.run_ok(&args(&["send-keys", "-t", &pane(slug), "Enter"]), None)
            .await
            .map(|_| ())
    }

    async fn interrupt(&self, slug: &str) -> Result<(), TmuxError> {
        self.run_ok(&args(&["send-keys", "-t", &pane(slug), "Escape"]), None)
            .await
            .map(|_| ())
    }

    async fn kill(&self, slug: &str) -> Result<(), TmuxError> {
        match self
            .run_ok(&args(&["kill-session", "-t", &session(slug)]), None)
            .await
        {
            Ok(_) => Ok(()),
            // Already gone is success; anything else surfaces.
            Err(e) if !self.exists(slug).await => {
                tracing::debug!(slug, error = %e, "kill: session already absent");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn detach_clients(&self, slug: &str) -> Result<(), TmuxError> {
        match self
            .run_ok(&args(&["detach-client", "-s", &session(slug)]), None)
            .await
        {
            Ok(_) => Ok(()),
            // No clients attached is the normal case, and reads as an error.
            Err(TmuxError::Failed { .. }) if self.clients(slug).await.is_empty() => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn attach_command(&self, slug: &str, read_only: bool) -> Vec<String> {
        let mut cmd = vec![
            self.bin.clone(),
            "-L".into(),
            self.socket.clone(),
            "attach-session".into(),
        ];
        if read_only {
            cmd.push("-r".into());
        }
        cmd.push("-t".into());
        cmd.push(session(slug));
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in `tmux` that appends its arguments, one per line, to `log`,
    /// then runs `body`.
    /// Writes an executable script without this process ever holding a write
    /// descriptor to it: a `sh` child writes it. With `std::fs::write`, a fork on
    /// another test thread can inherit that descriptor and keep it open until the
    /// forked child execs, and executing the script in that window fails with
    /// ETXTBSY ("Text file busy"). Renaming doesn't help: the inherited
    /// descriptor is on the same inode.
    fn write_executable(path: &Path, contents: &str) {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .args(["-c", r#"cat > "$0" && chmod 755 "$0""#])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh to write the script");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(contents.as_bytes())
            .expect("write the script");
        assert!(
            child.wait().expect("wait for sh").success(),
            "writing {} failed",
            path.display()
        );
    }

    fn fake_tmux(dir: &Path, body: &str) -> (TmuxRuntime, std::path::PathBuf) {
        let log = dir.join("calls.log");
        let script = dir.join("tmux");
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"CALL $*\" >> '{}'\nif [ \"$3\" = load-buffer ]; then cat >> '{}'.stdin; fi\n{body}\n",
                log.display(),
                log.display()
            ),
        );
        (TmuxRuntime::new(script.to_string_lossy(), "sock"), log)
    }

    fn calls(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[tokio::test]
    async fn start_passes_every_option_as_its_own_argument() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = fake_tmux(dir.path(), "exit 0");
        rt.start(
            "grp-1",
            Path::new("/wt/grp 1"),
            &["claude".into(), "--resume".into()],
            &[("A".into(), "b c".into())],
        )
        .await
        .unwrap();
        assert_eq!(
            calls(&log),
            vec![
                "CALL -L sock new-session -d -s grp-1 -c /wt/grp 1 -x 220 -y 50 -e A=b c claude --resume"
            ]
        );
    }

    #[tokio::test]
    async fn deliver_sends_text_on_stdin_and_never_as_an_argument() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, log) = fake_tmux(dir.path(), "exit 0");
        rt.deliver("grp", "line one\nline two; $(rm -rf /)")
            .await
            .unwrap();
        let c = calls(&log);
        assert_eq!(c[0], "CALL -L sock load-buffer -b conductor-grp -");
        assert_eq!(
            c[1],
            "CALL -L sock paste-buffer -p -d -b conductor-grp -t =grp:"
        );
        assert_eq!(c[2], "CALL -L sock send-keys -t =grp: Enter");
        assert!(c.iter().all(|l| !l.contains("rm -rf")), "{c:?}");
        let stdin = std::fs::read_to_string(dir.path().join("calls.log.stdin")).unwrap();
        assert_eq!(stdin, "line one\nline two; $(rm -rf /)");
    }

    #[tokio::test]
    async fn exists_and_clients_and_pane_pid() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"case "$3" in
has-session) exit 0;;
display) echo 4242;;
list-clients) printf '/dev/pts/1\n/dev/pts/2\n';;
esac"#;
        let (rt, _) = fake_tmux(dir.path(), body);
        assert!(rt.exists("g").await);
        assert_eq!(rt.pane_pid("g").await, Some(4242));
        assert_eq!(rt.clients("g").await, vec!["/dev/pts/1", "/dev/pts/2"]);
    }

    #[tokio::test]
    async fn missing_session_reports_absent() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = fake_tmux(dir.path(), "echo \"can't find session\" >&2; exit 1");
        assert!(!rt.exists("g").await);
        assert_eq!(rt.pane_pid("g").await, None);
        assert!(rt.clients("g").await.is_empty());
        // Idempotent: killing something already gone is fine.
        rt.kill("g").await.unwrap();
    }

    #[tokio::test]
    async fn kill_fails_when_the_session_survives() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"case "$3" in has-session) exit 0;; *) echo boom >&2; exit 1;; esac"#;
        let (rt, _) = fake_tmux(dir.path(), body);
        assert!(rt.kill("g").await.is_err());
    }

    #[tokio::test]
    async fn no_server_means_no_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = fake_tmux(dir.path(), "echo 'no server running on /tmp/x' >&2; exit 1");
        assert!(rt.list_sessions().await.unwrap().is_empty());
    }

    #[test]
    fn attach_targets_the_session_exactly_and_read_only_adds_r() {
        let rt = TmuxRuntime::new("tmux", "conductor");
        assert_eq!(
            rt.attach_command("grp", false),
            ["tmux", "-L", "conductor", "attach-session", "-t", "=grp"]
        );
        assert_eq!(
            rt.attach_command("grp", true),
            [
                "tmux",
                "-L",
                "conductor",
                "attach-session",
                "-r",
                "-t",
                "=grp"
            ]
        );
    }

    #[tokio::test]
    async fn detach_with_no_clients_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"case "$3" in detach-client) echo 'no client' >&2; exit 1;; esac"#;
        let (rt, log) = fake_tmux(dir.path(), body);
        rt.detach_clients("g").await.unwrap();
        assert_eq!(calls(&log)[0], "CALL -L sock detach-client -s =g");
    }

    #[tokio::test]
    async fn a_hung_tmux_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _) = fake_tmux(dir.path(), "exec sleep 30");
        let started = std::time::Instant::now();
        let r = rt.interrupt("g").await;
        assert!(matches!(r, Err(TmuxError::Timeout(_))), "{r:?}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "must not wait for the child"
        );
    }

    #[tokio::test]
    async fn missing_binary_is_a_spawn_error() {
        let rt = TmuxRuntime::new("/nonexistent/tmux", "s");
        assert!(matches!(rt.interrupt("g").await, Err(TmuxError::Spawn(_))));
        assert!(!rt.exists("g").await);
    }
}
