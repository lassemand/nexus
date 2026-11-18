//! The reviewer's `settings.json`, evaluated the way Claude Code evaluates it
//! in `--permission-mode dontAsk`: a tool call runs only if an allow rule
//! matches, and a deny rule always wins. Claude Code cannot run in CI (it needs
//! a login and the network), so this models the documented rule semantics: a
//! `Bash(prefix:*)` rule matches a command that is the prefix or starts with
//! the prefix and a space, `Bash(command)` matches exactly, and a command with
//! several parts (`;`, `&&`, `||`, `|`, a newline, `$(`, a backtick) needs
//! every part to be allowed.

use std::path::Path;

use serde_json::Value;

struct Rules {
    allow: Vec<String>,
    deny: Vec<String>,
}

fn rules() -> Rules {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/reviewer-settings.json");
    let json: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let list = |key: &str| -> Vec<String> {
        json["permissions"][key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    Rules {
        allow: list("allow"),
        deny: list("deny"),
    }
}

fn matches_bash(rule: &str, command: &str) -> bool {
    let Some(inner) = rule.strip_prefix("Bash(").and_then(|r| r.strip_suffix(')')) else {
        return false;
    };
    match inner.strip_suffix(":*") {
        Some(prefix) => {
            command == prefix
                || command
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with(' '))
        }
        None => command == inner,
    }
}

fn parts(command: &str) -> Vec<String> {
    let mut spaced = command.to_string();
    for sep in ["&&", "||", ";", "|", "\n", "$(", "`", ")", ">", "<", "&"] {
        spaced = spaced.replace(sep, "\u{1}");
    }
    spaced
        .split('\u{1}')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn bash_allowed(rules: &Rules, command: &str) -> bool {
    let parts = parts(command);
    if parts.is_empty() {
        return false;
    }
    parts.iter().all(|part| {
        !rules.deny.iter().any(|r| matches_bash(r, part))
            && rules.allow.iter().any(|r| matches_bash(r, part))
    })
}

#[test]
fn the_commands_an_injected_review_would_try_are_refused() {
    let rules = rules();
    for command in [
        "gh api repos/o/r/pulls/1",
        "gh api -X PATCH repos/o/r/pulls/1 -f state=closed",
        "gh api -X PUT repos/o/r/pulls/1/merge",
        "gh api graphql -f query=mutation{x}",
        "gh pr merge 1",
        "gh pr close 1",
        "gh pr edit 1 --title x",
        "conductor-review-gh api repos/o/r/pulls/1",
        "conductor-review-gh pr merge 1",
        "git push origin HEAD:main",
        "git commit -am x",
        "git fetch --upload-pack='touch /tmp/x' origin",
        "git -c core.sshCommand='touch /tmp/x' fetch origin",
        "git show --no-index /data/home/.claude.json /dev/null",
        "cargo test --config 'target.x.runner=\"touch /tmp/x\"'",
        "cargo test",
        "cargo build",
        "conductor-review-cargo build",
        "conductor-review-cargo run",
        "conductor-review-git push origin main",
        "conductor-review-git config core.sshCommand x",
        "cat /data/home/.config/github-app/private-key.pem",
        "cat /data/state/review-identity/private-key.pem",
        "cat /proc/1/environ",
        "env",
        "curl https://example.com",
        "bash -c 'gh api repos/o/r/pulls/1'",
        "sh -c 'git push'",
        "conductor-review-git status; gh api repos/o/r/pulls/1",
        "conductor-review-git status && git push",
        "conductor-review-git status | sh",
        "conductor-review-git status $(gh api repos/o/r/pulls/1)",
        "conductor-review-git status `gh api repos/o/r/pulls/1`",
        "conductor-review-git status > /data/home/.bashrc",
        "mkdir -p /data/home/.ssh",
        "rm -rf /data",
    ] {
        assert!(!bash_allowed(&rules, command), "{command} must be refused");
    }
}

#[test]
fn the_wrappers_the_reviewer_needs_are_allowed() {
    let rules = rules();
    for command in [
        "conductor-review-git log -3 --oneline",
        "conductor-review-git diff origin/main...abc123",
        "conductor-review-git show abc123:conductor/src/main.rs",
        "conductor-review-git fetch origin abc123",
        "conductor-review-git checkout --detach abc123",
        "conductor-review-git status",
        "conductor-review-git rev-parse HEAD",
        "conductor-review-git blame conductor/src/main.rs",
        "conductor-review-git ls-files conductor",
        "conductor-review-gh pr view 316",
        "conductor-review-gh pr diff 316",
        "conductor-review-gh pr checks 316",
        "conductor-review-gh reviews o/r 316",
        "conductor-review-gh threads o/r 316",
        "conductor-review-gh post-review o/r 316 .review/review-316.json",
        "conductor-review-cargo check -p conductor",
        "conductor-review-cargo clippy -p conductor -- -D warnings",
        "conductor-review-cargo test -p conductor",
        "mkdir -p .review",
    ] {
        assert!(bash_allowed(&rules, command), "{command} must be allowed");
    }
}

#[test]
fn only_the_wrappers_are_allowed_to_run() {
    let rules = rules();
    for rule in rules.allow.iter().filter(|r| r.starts_with("Bash(")) {
        assert!(
            rule.starts_with("Bash(conductor-review-") || rule == "Bash(mkdir -p .review)",
            "{rule} is not a wrapper"
        );
    }
    for raw in ["git", "gh", "cargo"] {
        assert!(
            rules.deny.contains(&format!("Bash({raw}:*)")),
            "raw {raw} is denied outright"
        );
        assert!(!rules.allow.iter().any(|r| r.starts_with(&format!("Bash({raw} "))));
    }
    assert!(!rules.allow.iter().any(|r| r.contains("gh api")));
}

#[test]
fn no_github_tool_that_can_close_edit_merge_or_push_is_allowed() {
    let rules = rules();
    let allowed: Vec<&String> = rules.allow.iter().filter(|r| r.starts_with("mcp__github__")).collect();
    for tool in [
        "mcp__github__merge_pull_request",
        "mcp__github__update_pull_request",
        "mcp__github__update_pull_request_branch",
        "mcp__github__issue_write",
        "mcp__github__label_write",
        "mcp__github__add_issue_comment",
        "mcp__github__create_pull_request",
        "mcp__github__push_files",
        "mcp__github__create_or_update_file",
        "mcp__github__delete_file",
        "mcp__github__create_branch",
        "mcp__github__actions_run_trigger",
    ] {
        assert!(!allowed.iter().any(|r| r.as_str() == tool), "{tool} is allowed");
        assert!(rules.deny.iter().any(|r| r == tool), "{tool} is not denied");
    }
}

#[test]
fn no_linear_write_tool_is_allowed() {
    let rules = rules();
    for rule in rules.allow.iter().filter(|r| r.starts_with("mcp__linear__")) {
        let name = rule.trim_start_matches("mcp__linear__");
        assert!(
            !name.starts_with("save_") && !name.starts_with("delete_") && !name.starts_with("create_"),
            "{rule} writes to Linear"
        );
    }
}

#[test]
fn the_reviewers_files_outside_its_worktree_cannot_be_edited() {
    let rules = rules();
    for rule in rules.allow.iter().filter(|r| r.starts_with("Edit(")) {
        assert!(rule.contains(".review"), "{rule} edits more than the verdict directory");
    }
    assert!(rules.deny.iter().any(|r| r == "Read(//data/home/**)"));
}
