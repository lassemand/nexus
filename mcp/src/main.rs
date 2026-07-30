use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::sync::Arc;
use tokio::io::{self, AsyncWriteExt};
use tokio::sync::Mutex;

// ── MCP stdio writer ──────────────────────────────────────────────────────────

/// Writes a JSON-RPC message to stdout (the MCP stdio transport).
/// Each message is a single line of JSON followed by a newline.
async fn write_mcp(stdout: &Arc<Mutex<io::Stdout>>, msg: Value) {
    let line = format!("{}\n", msg);
    let mut out = stdout.lock().await;
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.flush().await;
}

/// Send the MCP initialize response, declaring the claude/channel capability.
async fn send_initialize_response(stdout: &Arc<Mutex<io::Stdout>>, id: Value) {
    write_mcp(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "experimental": {
                        "claude/channel": {}
                    }
                },
                "serverInfo": {
                    "name": "linear-webhook",
                    "version": "0.1.0"
                },
                // Added to Claude's system prompt — tells it how to handle events
                "instructions": "Events from Linear arrive as <channel source=\"linear-webhook\" event=\"issue_todo\">. \
                    When a task moves to Todo, extract the issue id, title, labels, and description, \
                    then route to the appropriate agent based on label: \
                    Bug → @agent-debugger, Feature → @agent-builder, PRD → @agent-scoper. \
                    Begin work immediately. \
                    \
                    Events from GitHub arrive on the same channel as event=\"pr_comment\" or \
                    event=\"pr_review_comment\" — only for comments from a watched user (see \
                    GITHUB_WEBHOOK_USERS). The content is JSON: repo, pr_number, comment_id, author, \
                    body, url, and (for review comments) file_path/line/diff_hunk. Read the comment \
                    together with the PR's current diff. If what's being asked is clear and \
                    actionable, make the change, commit, and push a fix to the PR branch, then reply \
                    on the comment confirming what was fixed. If the comment is unclear or doesn't \
                    specify exactly what should change, do not guess — reply on the comment asking \
                    for clarification instead."
            }
        }),
    )
    .await;
}

/// Push a Linear issue into the Claude Code session as a channel event.
async fn send_channel_notification(stdout: &Arc<Mutex<io::Stdout>>, issue: &LinearIssue) {
    let content = serde_json::to_string(issue).unwrap_or_default();
    write_mcp(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {
                "content": content,
                "meta": {
                    "event": "issue_todo",
                    "issue_id": issue.identifier
                }
            }
        }),
    )
    .await;
}

// ── Linear webhook payload types ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LinearWebhook {
    #[serde(rename = "type")]
    event_type: Option<String>,
    data: Option<LinearData>,
}

#[derive(Debug, Deserialize)]
struct LinearData {
    id: Option<String>,
    identifier: Option<String>,
    title: Option<String>,
    description: Option<String>,
    state: Option<LinearState>,
    labels: Option<Vec<LinearLabel>>,
}

#[derive(Debug, Deserialize)]
struct LinearState {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LinearLabel {
    name: String,
}

#[derive(Debug, Serialize)]
struct LinearIssue {
    identifier: String,
    title: String,
    description: String,
    labels: Vec<String>,
}

// ── GitHub webhook payload types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GithubUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GithubRepository {
    full_name: String,
}

/// Payload for the `issue_comment` event. GitHub fires this for comments on
/// both plain issues and PRs — `issue.pull_request` is only `Some` when the
/// comment is actually on a PR, which is how we tell the two apart.
#[derive(Debug, Deserialize)]
struct IssueCommentPayload {
    action: Option<String>,
    issue: Option<IssueCommentIssue>,
    comment: Option<IssueCommentBody>,
    repository: Option<GithubRepository>,
}

#[derive(Debug, Deserialize)]
struct IssueCommentIssue {
    number: u64,
    pull_request: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct IssueCommentBody {
    id: u64,
    body: String,
    html_url: String,
    user: GithubUser,
}

/// Payload for the `pull_request_review_comment` event — an inline comment
/// left on a specific line of the diff.
#[derive(Debug, Deserialize)]
struct ReviewCommentPayload {
    action: Option<String>,
    pull_request: Option<ReviewCommentPr>,
    comment: Option<ReviewCommentBody>,
    repository: Option<GithubRepository>,
}

#[derive(Debug, Deserialize)]
struct ReviewCommentPr {
    number: u64,
}

#[derive(Debug, Deserialize)]
struct ReviewCommentBody {
    id: u64,
    body: String,
    html_url: String,
    user: GithubUser,
    path: Option<String>,
    line: Option<u64>,
    diff_hunk: Option<String>,
}

/// Unified shape sent to Claude regardless of which GitHub event produced it.
#[derive(Debug, Serialize)]
struct GithubPrComment {
    repo: String,
    pr_number: u64,
    comment_id: u64,
    author: String,
    body: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff_hunk: Option<String>,
}

/// Push a GitHub PR comment into the Claude Code session as a channel event.
async fn send_github_channel_notification(
    stdout: &Arc<Mutex<io::Stdout>>,
    event: &str,
    comment: &GithubPrComment,
) {
    let content = serde_json::to_string(comment).unwrap_or_default();
    write_mcp(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {
                "content": content,
                "meta": {
                    "event": event,
                    "repo": comment.repo,
                    "pr_number": comment.pr_number.to_string(),
                    "comment_id": comment.comment_id.to_string()
                }
            }
        }),
    )
    .await;
}

/// Verifies GitHub's `X-Hub-Signature-256` header: `sha256=<hex hmac>` of the
/// raw request body, keyed by the webhook secret configured in the GitHub
/// repo settings. Must run against the *raw* bytes, before any JSON parsing.
fn verify_github_signature(secret: &str, signature_header: &str, body: &[u8]) -> bool {
    let Some(sig_hex) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(sig_bytes) = hex::decode(sig_hex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig_bytes).is_ok()
}

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    stdout: Arc<Mutex<io::Stdout>>,
    /// Verifies `X-Hub-Signature-256` on incoming GitHub webhooks when set
    /// (`GITHUB_WEBHOOK_SECRET` env var). `None` skips verification — fine for
    /// local dev, but the real deployment should always set this.
    github_webhook_secret: Option<String>,
    /// Only comments from these GitHub logins (case-insensitive) wake the
    /// agent — otherwise every human back-and-forth on a PR would trigger it.
    /// Configurable via `GITHUB_WEBHOOK_USERS` (comma-separated); defaults to
    /// just `lassemand`.
    watched_github_users: Vec<String>,
}

// ── HTTP handler ──────────────────────────────────────────────────────────────

async fn handle_webhook(State(state): State<AppState>, body: axum::body::Bytes) -> StatusCode {
    let payload: LinearWebhook = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[linear-channel] Failed to parse body: {e}");
            return StatusCode::BAD_REQUEST;
        }
    };

    // Only handle Issue events
    if payload.event_type.as_deref() != Some("Issue") {
        return StatusCode::OK;
    }

    let data = match payload.data {
        Some(d) => d,
        None => return StatusCode::OK,
    };

    // Only forward when issue moves to Todo
    let state_name = data
        .state
        .as_ref()
        .and_then(|s| s.name.as_deref())
        .unwrap_or("");
    if state_name != "Todo" {
        return StatusCode::OK;
    }

    let issue = LinearIssue {
        identifier: data
            .identifier
            .unwrap_or_else(|| data.id.unwrap_or_default()),
        title: data.title.unwrap_or_default(),
        description: data.description.unwrap_or_default(),
        labels: data
            .labels
            .unwrap_or_default()
            .into_iter()
            .map(|l| l.name)
            .collect(),
    };

    eprintln!(
        "[linear-channel] Forwarding issue {} to Claude",
        issue.identifier
    );
    send_channel_notification(&state.stdout, &issue).await;

    StatusCode::OK
}

// ── GitHub webhook handler ──────────────────────────────────────────────────────

fn is_watched_github_user(state: &AppState, login: &str) -> bool {
    state
        .watched_github_users
        .iter()
        .any(|u| u.eq_ignore_ascii_case(login))
}

async fn handle_github_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    if let Some(secret) = &state.github_webhook_secret {
        let signature = headers
            .get("X-Hub-Signature-256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !verify_github_signature(secret, signature, &body) {
            eprintln!("[github-channel] signature verification failed, dropping event");
            return StatusCode::UNAUTHORIZED;
        }
    }

    let event = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    match event.as_str() {
        "issue_comment" => handle_issue_comment(&state, &body).await,
        "pull_request_review_comment" => handle_review_comment(&state, &body).await,
        _ => StatusCode::OK,
    }
}

async fn handle_issue_comment(state: &AppState, body: &[u8]) -> StatusCode {
    let payload: IssueCommentPayload = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[github-channel] failed to parse issue_comment payload: {e}");
            return StatusCode::BAD_REQUEST;
        }
    };

    if payload.action.as_deref() != Some("created") {
        return StatusCode::OK;
    }
    let Some(issue) = payload.issue else {
        return StatusCode::OK;
    };
    // `issue_comment` fires for plain issues too; only PRs carry `pull_request`.
    if issue.pull_request.is_none() {
        return StatusCode::OK;
    }
    let Some(comment) = payload.comment else {
        return StatusCode::OK;
    };
    if !is_watched_github_user(state, &comment.user.login) {
        return StatusCode::OK;
    }

    let event = GithubPrComment {
        repo: payload.repository.map(|r| r.full_name).unwrap_or_default(),
        pr_number: issue.number,
        comment_id: comment.id,
        author: comment.user.login,
        body: comment.body,
        url: comment.html_url,
        file_path: None,
        line: None,
        diff_hunk: None,
    };

    eprintln!(
        "[github-channel] forwarding PR comment {} on {}#{} to Claude",
        event.comment_id, event.repo, event.pr_number
    );
    send_github_channel_notification(&state.stdout, "pr_comment", &event).await;

    StatusCode::OK
}

async fn handle_review_comment(state: &AppState, body: &[u8]) -> StatusCode {
    let payload: ReviewCommentPayload = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[github-channel] failed to parse pull_request_review_comment payload: {e}");
            return StatusCode::BAD_REQUEST;
        }
    };

    if payload.action.as_deref() != Some("created") {
        return StatusCode::OK;
    }
    let Some(pr) = payload.pull_request else {
        return StatusCode::OK;
    };
    let Some(comment) = payload.comment else {
        return StatusCode::OK;
    };
    if !is_watched_github_user(state, &comment.user.login) {
        return StatusCode::OK;
    }

    let event = GithubPrComment {
        repo: payload.repository.map(|r| r.full_name).unwrap_or_default(),
        pr_number: pr.number,
        comment_id: comment.id,
        author: comment.user.login,
        body: comment.body,
        url: comment.html_url,
        file_path: comment.path,
        line: comment.line,
        diff_hunk: comment.diff_hunk,
    };

    eprintln!(
        "[github-channel] forwarding PR review comment {} on {}#{} to Claude",
        event.comment_id, event.repo, event.pr_number
    );
    send_github_channel_notification(&state.stdout, "pr_review_comment", &event).await;

    StatusCode::OK
}

// ── MCP stdio loop ────────────────────────────────────────────────────────────

/// Reads JSON-RPC messages from stdin and handles initialize / ping.
/// Claude Code spawns this binary as a subprocess and communicates over stdio.
async fn stdio_loop(stdout: Arc<Mutex<io::Stdout>>) {
    use tokio::io::AsyncBufReadExt;
    let stdin = io::BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => {
                send_initialize_response(&stdout, id).await;
            }
            "ping" => {
                write_mcp(&stdout, json!({ "jsonrpc": "2.0", "id": id, "result": {} })).await;
            }
            "notifications/initialized" => {
                // Claude Code sends this after initialize — no response needed
            }
            _ => {
                // Return method-not-found for unknown requests that have an id
                if id != Value::Null {
                    write_mcp(
                        &stdout,
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": "Method not found" }
                        }),
                    )
                    .await;
                }
            }
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let stdout = Arc::new(Mutex::new(io::stdout()));

    let github_webhook_secret = std::env::var("GITHUB_WEBHOOK_SECRET").ok();
    if github_webhook_secret.is_none() {
        eprintln!(
            "[github-channel] GITHUB_WEBHOOK_SECRET not set — skipping signature verification"
        );
    }
    let watched_github_users = std::env::var("GITHUB_WEBHOOK_USERS")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["lassemand".to_string()]);

    let state = AppState {
        stdout: stdout.clone(),
        github_webhook_secret,
        watched_github_users,
    };

    // HTTP server on port 8788 — receives Linear webhooks (/webhook) and
    // GitHub PR comment webhooks (/webhook/github)
    let app = Router::new()
        .route("/webhook", post(handle_webhook))
        .route("/webhook/github", post(handle_github_webhook))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8788")
        .await
        .expect("Failed to bind port 8788");

    eprintln!("[linear-channel] HTTP listening on 127.0.0.1:8788");

    // Run both the MCP stdio loop and the HTTP server concurrently
    tokio::select! {
        _ = stdio_loop(stdout) => {
            eprintln!("[linear-channel] stdio closed, exiting");
        }
        result = axum::serve(listener, app) => {
            if let Err(e) = result {
                eprintln!("[linear-channel] HTTP error: {e}");
            }
        }
    }
}
