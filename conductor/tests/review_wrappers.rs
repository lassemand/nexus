//! The wrappers a review session is allowed to run (`conductor/deploy/review-bin`):
//! each rejected form must exit non-zero and must never reach the program it
//! wraps, which is checked with a marker file the fake program writes.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let s = Sandbox { dir };
        fs::create_dir_all(s.identity()).unwrap();
        fs::write(s.identity().join("gitconfig"), "[user]\n\tname = t\n").unwrap();
        fs::write(
            s.identity().join("app.env"),
            "REVIEW_APP_ID=1\nREVIEW_INSTALLATION_ID=2\n",
        )
        .unwrap();
        fs::create_dir_all(s.worktree().join(".review")).unwrap();
        s.script(
            "fake-real",
            "#!/bin/sh\necho \"$@\" > \"$MARKER\"\nenv > \"$MARKER.env\"\n",
        );
        s.script(
            "fake-helper",
            "#!/bin/sh\ncat >/dev/null\nprintf 'username=x-access-token\\npassword=tok\\n'\n",
        );
        s
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn identity(&self) -> PathBuf {
        self.path().join("identity")
    }
    fn worktree_root(&self) -> PathBuf {
        self.path().join("worktrees")
    }
    fn worktree(&self) -> PathBuf {
        self.worktree_root().join("review-nex-1")
    }
    fn marker(&self) -> PathBuf {
        self.path().join("marker")
    }

    fn script(&self, name: &str, body: &str) {
        let p = self.path().join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run(&self, wrapper: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("deploy/review-bin")
            .join(wrapper);
        let fake = self.path().join("fake-real");
        let mut cmd = Command::new(script);
        cmd.args(args)
            .current_dir(self.worktree())
            .env("MARKER", self.marker())
            .env("CONDUCTOR_REVIEW_IDENTITY_DIR", self.identity())
            .env("CONDUCTOR_REVIEW_WORKTREE_ROOT", self.worktree_root())
            .env("CONDUCTOR_REVIEW_REAL_GIT", &fake)
            .env("CONDUCTOR_REVIEW_REAL_CARGO", &fake)
            .env("CONDUCTOR_REVIEW_REAL_GH", &fake)
            .env(
                "GITHUB_APP_CREDENTIAL_HELPER",
                self.path().join("fake-helper"),
            )
            .env(
                "CONDUCTOR_REVIEW_CARGO_HOME",
                self.path().join("cargo-home"),
            );
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn reached_real_program(&self) -> bool {
        self.marker().exists()
    }
}

fn assert_rejected(wrapper: &str, args: &[&str]) {
    assert_rejected_with(wrapper, args, &[]);
}

fn assert_rejected_with(wrapper: &str, args: &[&str], env: &[(&str, &str)]) {
    let sb = Sandbox::new();
    let out = sb.run(wrapper, args, env);
    assert!(
        !out.status.success(),
        "{wrapper} {args:?} should be rejected, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !sb.reached_real_program(),
        "{wrapper} {args:?} reached the wrapped program"
    );
}

fn assert_allowed(wrapper: &str, args: &[&str]) -> Sandbox {
    let sb = Sandbox::new();
    let out = sb.run(wrapper, args, &[]);
    assert!(
        out.status.success(),
        "{wrapper} {args:?} should be allowed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(sb.reached_real_program());
    sb
}

#[test]
fn git_rejects_every_form_that_names_a_program_or_a_file() {
    for args in [
        &["fetch", "--upload-pack=touch /tmp/x", "origin"][..],
        &["fetch", "--upload-pack", "touch /tmp/x", "origin"],
        &["fetch", "origin", "--receive-pack=x"],
        &["fetch", "--exec-path=/tmp"],
        &["fetch", "ext::sh -c touch% /tmp/x"],
        &["fetch", "https://example.com/x.git"],
        &["fetch"],
        &["-c", "core.sshCommand=touch /tmp/x", "fetch", "origin"],
        &["-c", "core.fsmonitor=touch /tmp/x", "status"],
        &["status", "-c", "core.fsmonitor=touch /tmp/x"],
        &["--exec-path=/tmp", "status"],
        &["diff", "--no-index", "/etc/passwd", "/dev/null"],
        &["diff", "--output=/tmp/x"],
        &["log", "--output=/tmp/x"],
        &["show", "--ext-diff"],
        &["show", "--textconv"],
        &["--git-dir=/tmp", "status"],
        &["checkout", "--recurse-submodules"],
        &["push", "origin", "main"],
        &["commit", "-m", "x"],
        &["config", "core.sshCommand", "x"],
        &["clone", "x"],
        &["remote", "add", "x", "y"],
        &[],
    ] {
        assert_rejected("conductor-review-git", args);
    }
}

#[test]
fn git_runs_allowed_subcommands_with_its_own_environment() {
    for sub in [
        "log",
        "diff",
        "show",
        "checkout",
        "blame",
        "ls-files",
        "status",
        "rev-parse",
    ] {
        assert_allowed("conductor-review-git", &[sub, "HEAD"]);
    }
    let sb = assert_allowed("conductor-review-git", &["fetch", "origin", "abc123"]);
    assert!(fs::read_to_string(sb.marker())
        .unwrap()
        .contains("--no-pager fetch origin abc123"));
}

#[test]
fn git_drops_environment_overrides_that_select_a_program() {
    let sb = Sandbox::new();
    let out = sb.run(
        "conductor-review-git",
        &["status"],
        &[
            ("GIT_SSH_COMMAND", "touch /tmp/x"),
            ("GIT_EXEC_PATH", "/tmp"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "core.sshCommand"),
            ("GIT_CONFIG_VALUE_0", "touch /tmp/x"),
            ("GIT_EXTERNAL_DIFF", "touch /tmp/x"),
            ("LD_PRELOAD", "/tmp/x.so"),
        ],
    );
    assert!(out.status.success());
    let env = fs::read_to_string(format!("{}.env", sb.marker().display())).unwrap();
    for gone in [
        "GIT_SSH_COMMAND",
        "GIT_EXEC_PATH",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_EXTERNAL_DIFF",
        "LD_PRELOAD",
    ] {
        assert!(!env.contains(gone), "{gone} survived: {env}");
    }
    assert!(env.contains(&format!(
        "GIT_CONFIG_GLOBAL={}",
        sb.identity().join("gitconfig").display()
    )));
}

#[test]
fn cargo_rejects_every_form_that_names_a_program() {
    for args in [
        &[
            "test",
            "--config",
            "target.x86_64-unknown-linux-gnu.runner='touch /tmp/x'",
        ][..],
        &["test", "--config=build.rustc-wrapper='touch /tmp/x'"],
        &["check", "--config", "x"],
        &["clippy", "-Zunstable-options"],
        &["test", "-Z", "unstable-options"],
        &["test", "--manifest-path", "/etc/Cargo.toml"],
        &["test", "--manifest-path=/etc/Cargo.toml"],
        &["test", "--manifest-path", "../../other/Cargo.toml"],
        &["test", "--manifest-path"],
        &["test", "--target-dir", "/tmp/x"],
        &["test", "+nightly"],
        &["clippy", "--", "-Clinker=/tmp/x"],
        &["clippy", "--", "-C", "link-arg=x"],
        &["build"],
        &["run"],
        &["install", "x"],
        &["publish"],
        &["fetch"],
        &[],
    ] {
        assert_rejected("conductor-review-cargo", args);
    }
}

#[test]
fn cargo_runs_check_clippy_and_test_inside_the_worktree() {
    for args in [
        &["check", "-p", "conductor"][..],
        &["clippy", "-p", "conductor", "--", "-D", "warnings"],
        &["test", "-p", "conductor", "--", "--nocapture"],
        &["test", "--manifest-path", "Cargo.toml"],
    ] {
        assert_allowed("conductor-review-cargo", args);
    }
}

#[test]
fn cargo_refuses_a_worktree_with_a_cargo_config() {
    for name in ["config.toml", "config"] {
        let sb = Sandbox::new();
        fs::create_dir_all(sb.worktree().join(".cargo")).unwrap();
        fs::write(
            sb.worktree().join(".cargo").join(name),
            "[target.x86_64-unknown-linux-gnu]\nrunner = \"touch /tmp/x\"\n",
        )
        .unwrap();
        let out = sb.run("conductor-review-cargo", &["test"], &[]);
        assert!(!out.status.success());
        assert!(!sb.reached_real_program());
    }
}

#[test]
fn cargo_refuses_to_run_outside_a_review_worktree() {
    assert_rejected_with(
        "conductor-review-cargo",
        &["test"],
        &[("CONDUCTOR_REVIEW_WORKTREE_ROOT", "/nonexistent")],
    );
}

#[test]
fn cargo_drops_environment_overrides_but_keeps_the_target_dir() {
    let sb = Sandbox::new();
    let out = sb.run(
        "conductor-review-cargo",
        &["check"],
        &[
            ("CARGO_TARGET_DIR", "/review/target"),
            ("CARGO_BUILD_RUSTC_WRAPPER", "touch /tmp/x"),
            (
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER",
                "touch /tmp/x",
            ),
            ("RUSTC_WRAPPER", "touch /tmp/x"),
            ("RUSTC", "/tmp/x"),
            ("RUSTDOC", "/tmp/x"),
            ("RUSTFLAGS", "-Clinker=/tmp/x"),
            ("LD_PRELOAD", "/tmp/x.so"),
            ("GH_TOKEN", "secret"),
            ("ANTHROPIC_API_KEY", "secret"),
        ],
    );
    assert!(out.status.success());
    let env = fs::read_to_string(format!("{}.env", sb.marker().display())).unwrap();
    assert!(env.contains("CARGO_TARGET_DIR=/review/target"));
    for gone in [
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_TARGET_X86_64",
        "RUSTC_WRAPPER",
        "RUSTC=",
        "RUSTDOC",
        "RUSTFLAGS",
        "LD_PRELOAD",
        "GH_TOKEN",
        "ANTHROPIC_API_KEY",
    ] {
        assert!(!env.contains(gone), "{gone} survived: {env}");
    }
}

#[test]
fn gh_has_no_api_and_no_way_to_close_edit_or_merge() {
    for args in [
        &["api", "repos/o/r/pulls/1"][..],
        &[
            "api",
            "-X",
            "PATCH",
            "repos/o/r/pulls/1",
            "-f",
            "state=closed",
        ],
        &["api", "--method", "PUT", "repos/o/r/pulls/1/merge"],
        &["api", "graphql", "-f", "query=mutation{closePullRequest}"],
        &["api", "-X", "POST", "repos/o/r/issues/1/labels"],
        &["pr", "merge", "1"],
        &["pr", "close", "1"],
        &["pr", "edit", "1", "--title", "x"],
        &["pr", "comment", "1", "-b", "x"],
        &["pr", "review", "1", "--approve"],
        &["pr", "ready", "1"],
        &["pr", "view", "1", "--web"],
        &["issue", "close", "1"],
        &["label", "create", "x"],
        &["repo", "delete", "o/r"],
        &["auth", "token"],
        &["alias", "set", "x", "y"],
        &["extension", "install", "x"],
        &["reviews", "o/r", "abc"],
        &["reviews", "o/r;rm", "1"],
        &["reviews", "o/r/../x", "1"],
        &["threads", "o", "1"],
        &["pr", "view", "1", "-R", "evil/other"],
        &[],
    ] {
        assert_rejected_with(
            "conductor-review-gh",
            args,
            &[("CONDUCTOR_REVIEW_REPO", "o/r")],
        );
    }
}

#[test]
fn gh_post_review_takes_only_the_review_payload() {
    let payload = r#"{"commit_id":"abcdef1","event":"APPROVE","body":"b","comments":[]}"#;
    let write = |sb: &Sandbox, name: &str, body: &str| {
        fs::write(sb.worktree().join(".review").join(name), body).unwrap();
    };
    for (name, body, file) in [
        (
            "review-7.json",
            r#"{"event":"APPROVE","commit_id":"abcdef1","state":"closed"}"#,
            ".review/review-7.json",
        ),
        (
            "review-7.json",
            r#"{"event":"DISMISS","commit_id":"abcdef1"}"#,
            ".review/review-7.json",
        ),
        (
            "review-7.json",
            r#"{"event":"APPROVE","commit_id":"not a sha"}"#,
            ".review/review-7.json",
        ),
        ("review-7.json", "not json", ".review/review-7.json"),
        ("review-8.json", payload, ".review/review-7.json"),
        ("review-7.json", payload, "/etc/passwd"),
        ("review-7.json", payload, ".review/../review-7.json"),
        ("review-7.json", payload, "../.review/review-7.json"),
    ] {
        let sb = Sandbox::new();
        write(&sb, name, body);
        let out = sb.run(
            "conductor-review-gh",
            &["post-review", "o/r", "7", file],
            &[],
        );
        assert!(!out.status.success(), "{name} {body} {file}");
        assert!(!sb.reached_real_program(), "{name} {body} {file}");
    }
    let sb = Sandbox::new();
    write(&sb, "review-7.json", payload);
    let out = sb.run(
        "conductor-review-gh",
        &["post-review", "o/r", "7", ".review/review-7.json"],
        &[],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let call = fs::read_to_string(sb.marker()).unwrap();
    assert!(
        call.contains("api --method POST repos/o/r/pulls/7/reviews --input .review/review-7.json")
    );
}

#[test]
fn gh_post_review_refuses_a_symlinked_payload() {
    let sb = Sandbox::new();
    let target = sb.path().join("elsewhere.json");
    fs::write(&target, r#"{"event":"APPROVE","commit_id":"abcdef1"}"#).unwrap();
    std::os::unix::fs::symlink(&target, sb.worktree().join(".review/review-7.json")).unwrap();
    let out = sb.run(
        "conductor-review-gh",
        &["post-review", "o/r", "7", ".review/review-7.json"],
        &[],
    );
    assert!(!out.status.success());
    assert!(!sb.reached_real_program());
}

#[test]
fn gh_reads_use_fixed_requests_and_a_per_call_token() {
    let sb = Sandbox::new();
    let out = sb.run("conductor-review-gh", &["reviews", "o/r", "7"], &[]);
    assert!(out.status.success());
    assert_eq!(
        fs::read_to_string(sb.marker()).unwrap().trim(),
        "api --method GET --paginate repos/o/r/pulls/7/reviews"
    );
    let env = fs::read_to_string(format!("{}.env", sb.marker().display())).unwrap();
    assert!(env.contains("GH_TOKEN=tok"));

    let sb = Sandbox::new();
    let out = sb.run("conductor-review-gh", &["threads", "o/r", "7"], &[]);
    assert!(out.status.success());
    let call = fs::read_to_string(sb.marker()).unwrap();
    assert!(call.starts_with("api graphql -f query=query("));
    assert!(!call.contains("mutation"));

    for args in [
        &["pr", "view", "7"][..],
        &["pr", "diff", "7"],
        &["pr", "checks", "7", "-R", "o/r"],
    ] {
        assert_allowed("conductor-review-gh", args);
    }
}

#[test]
fn gh_reads_do_not_run_without_an_identity() {
    let sb = Sandbox::new();
    fs::remove_file(sb.identity().join("app.env")).unwrap();
    let out = sb.run("conductor-review-gh", &["reviews", "o/r", "7"], &[]);
    assert!(!out.status.success());
    assert!(!sb.reached_real_program());
}
