//! A stand-in for an interactive `claude`, for tests that run real tmux.
//!
//! It behaves like the parts of Claude Code that conductor depends on:
//!
//! * `--session-id <uuid>` / `--resume <uuid>` name the session; everything
//!   else on the command line is accepted and ignored;
//! * on start it runs `conductor hook` with a `SessionStart` payload;
//! * it puts the terminal in raw mode, turns on bracketed paste, and reads
//!   keystrokes. A pasted prompt followed by Enter is a turn: `UserPromptSubmit`,
//!   work for `FAKE_CLAUDE_TURN_MS`, then `Stop`;
//! * Escape during a turn stops the work, and sends `Stop` only when
//!   `FAKE_CLAUDE_STOP_ON_INTERRUPT=1`, covering both behaviours Claude might
//!   have (whether it fires `Stop` on an interrupt is not verified);
//! * `FAKE_CLAUDE_IGNORE_ENTER=1` swallows Enter, as a paste that never submits;
//! * `/exit` or end of input sends `SessionEnd` and exits.
//!
//! Everything it does is appended as a JSON line to `$FAKE_CLAUDE_LOG`, so a test
//! can read order, overlap and exact prompt text from what the process saw.
//!
//! Environment: `FAKE_CLAUDE_LOG` (required), `FAKE_CLAUDE_HOOK_BIN` (the
//! `conductor` binary, default `conductor`), `FAKE_CLAUDE_TURN_MS` (default 200),
//! `FAKE_CLAUDE_STOP_ON_INTERRUPT`, `FAKE_CLAUDE_IGNORE_ENTER`.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use nix::sys::termios::{self, SetArg};

/// How long after an ESC byte to wait for the rest of an escape sequence
/// before calling it a bare Escape key.
const ESC_WAIT: Duration = Duration::from_millis(40);

struct Fake {
    session_id: String,
    log_path: String,
    hook_bin: String,
    turn: Duration,
    stop_on_interrupt: bool,
    ignore_enter: bool,
    input: Vec<u8>,
    in_paste: bool,
    paste_started_at: usize,
    last_was_cr: bool,
    rx: Receiver<Option<u8>>,
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut session_id = String::new();
    let mut resumed = false;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--session-id" | "--resume" => {
                resumed = argv[i] == "--resume";
                session_id = argv.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    // Raw mode: no echo, no line editing, Enter arrives as CR. Failure is
    // ignored so the fake still runs when stdin is not a terminal.
    let stdin = std::io::stdin();
    if let Ok(mut t) = termios::tcgetattr(&stdin) {
        termios::cfmakeraw(&mut t);
        let _ = termios::tcsetattr(&stdin, SetArg::TCSANOW, &t);
    }
    // Ask for bracketed paste, as Claude Code does. tmux only wraps a paste in
    // the markers for a pane that has asked.
    print!("\x1b[?2004h");
    let _ = std::io::stdout().flush();

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(1) => {
                    if tx.send(Some(buf[0])).is_err() {
                        return;
                    }
                }
                _ => {
                    let _ = tx.send(None);
                    return;
                }
            }
        }
    });

    let mut fake = Fake {
        session_id,
        log_path: std::env::var("FAKE_CLAUDE_LOG").expect("FAKE_CLAUDE_LOG"),
        hook_bin: std::env::var("FAKE_CLAUDE_HOOK_BIN").unwrap_or_else(|_| "conductor".into()),
        turn: Duration::from_millis(
            std::env::var("FAKE_CLAUDE_TURN_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(200),
        ),
        stop_on_interrupt: flag("FAKE_CLAUDE_STOP_ON_INTERRUPT"),
        ignore_enter: flag("FAKE_CLAUDE_IGNORE_ENTER"),
        input: Vec::new(),
        in_paste: false,
        paste_started_at: 0,
        last_was_cr: false,
        rx,
    };

    fake.log(serde_json::json!({
        "event": "started",
        "resume": resumed,
        "argv": argv,
        "cwd": std::env::current_dir().ok(),
    }));
    fake.hook("SessionStart");
    fake.run();
}

impl Fake {
    fn log(&self, mut line: serde_json::Value) {
        line["session_id"] = self.session_id.clone().into();
        line["pid"] = std::process::id().into();
        line["ts"] = now().into();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .expect("open FAKE_CLAUDE_LOG");
        // A single write of the whole line: `writeln!` makes several, and with
        // eight fakes appending at once their pieces would interleave.
        f.write_all(format!("{line}\n").as_bytes())
            .expect("write FAKE_CLAUDE_LOG");
    }

    /// Runs `conductor hook` with a payload for this session, as Claude would.
    fn hook(&self, name: &str) {
        let payload = serde_json::json!({
            "hook_event_name": name,
            "session_id": self.session_id,
            "cwd": std::env::current_dir().ok(),
        });
        let child = Command::new(&self.hook_bin)
            .arg("hook")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(payload.to_string().as_bytes());
            }
            let _ = child.wait();
        }
    }

    fn say(&self, text: &str) {
        print!("{text}\r\n");
        let _ = std::io::stdout().flush();
    }

    fn exit(&self) -> ! {
        self.hook("SessionEnd");
        std::process::exit(0);
    }

    /// The idle prompt: read keys until a turn is submitted, forever.
    fn run(&mut self) -> ! {
        loop {
            match self.rx.recv() {
                Ok(Some(b)) => self.key(b),
                _ => self.exit(),
            }
        }
    }

    /// Handles one byte at the idle prompt.
    fn key(&mut self, b: u8) {
        match b {
            0x1b => match self.escape() {
                Esc::Key => {} // A bare Escape at an idle prompt does nothing.
                Esc::PasteStart => {
                    self.in_paste = true;
                    self.paste_started_at = self.input.len();
                    self.last_was_cr = false;
                }
                Esc::PasteEnd => {
                    self.in_paste = false;
                    let text =
                        String::from_utf8_lossy(&self.input[self.paste_started_at..]).into_owned();
                    self.log(serde_json::json!({"event": "paste", "prompt": text}));
                }
                Esc::Other => {}
            },
            // Inside a paste, CR and LF are both a newline (tmux turns LF into
            // CR), and CRLF counts once.
            b'\r' | b'\n' if self.in_paste => {
                if !(b == b'\n' && self.last_was_cr) {
                    self.input.push(b'\n');
                }
                self.last_was_cr = b == b'\r';
            }
            b'\r' | b'\n' => self.enter(),
            0x7f => {
                self.input.pop();
            }
            // Ctrl-D on an empty box is end of input.
            0x04 if self.input.is_empty() => self.exit(),
            _ => {
                self.last_was_cr = false;
                self.input.push(b);
            }
        }
    }

    /// Reads the rest of an escape sequence after ESC.
    fn escape(&mut self) -> Esc {
        match self.rx.recv_timeout(ESC_WAIT) {
            Err(RecvTimeoutError::Timeout) => Esc::Key,
            Ok(Some(b'[')) => {
                let mut seq = Vec::new();
                while let Ok(Some(b)) = self.rx.recv_timeout(ESC_WAIT) {
                    seq.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        break;
                    }
                }
                match seq.as_slice() {
                    b"200~" => Esc::PasteStart,
                    b"201~" => Esc::PasteEnd,
                    _ => Esc::Other,
                }
            }
            _ => Esc::Other,
        }
    }

    fn enter(&mut self) {
        if self.ignore_enter {
            self.log(serde_json::json!({"event": "enter_ignored"}));
            return;
        }
        let prompt = String::from_utf8_lossy(&std::mem::take(&mut self.input)).into_owned();
        if prompt.trim() == "/exit" {
            self.exit();
        }
        self.turn(prompt);
    }

    /// One turn: submit, work, then stop (or stop early on Escape).
    fn turn(&mut self, prompt: String) {
        self.hook("UserPromptSubmit");
        // Logged once the hook has returned, so the registry already says Busy
        // when the log says the turn began, and "end" is logged before `Stop`
        // is sent: a logged turn always lies inside the registry's Busy window.
        self.log(serde_json::json!({"event": "start", "prompt": prompt}));
        self.say(&format!("working on {} bytes", prompt.len()));

        let deadline = Instant::now() + self.turn;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.log(serde_json::json!({"event": "end"}));
                self.hook("Stop");
                return;
            }
            match self.rx.recv_timeout(left) {
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) | Ok(None) => self.exit(),
                Ok(Some(0x1b)) => {
                    if let Esc::Key = self.escape() {
                        self.log(serde_json::json!({"event": "interrupted"}));
                        if self.stop_on_interrupt {
                            self.log(serde_json::json!({"event": "end", "interrupted": true}));
                            self.hook("Stop");
                        }
                        return;
                    }
                }
                // Typing during a turn is queued in the box, as in Claude.
                Ok(Some(b)) if !matches!(b, b'\r' | b'\n') => self.input.push(b),
                Ok(Some(_)) => {}
            }
        }
    }
}

enum Esc {
    Key,
    PasteStart,
    PasteEnd,
    Other,
}
