use axum::{extract::State, http::StatusCode, routing::post, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{self, AsyncWriteExt};
use tokio::sync::Mutex;

// ── MCP stdio writer ──────────────────────────────────────────────────────────

/// Writes a JSON-RPC message to stdout (the MCP stdio transport).
/// Each message is a single line of JSON followed by a newline.
async fn write_mcp(stdout: &Arc<Mutex<io::Stdout>>, msg: Value) {
    let line = format!("{}\n", msg.to_string());
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
                    Begin work immediately."
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

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    stdout: Arc<Mutex<io::Stdout>>,
}

// ── HTTP handler ──────────────────────────────────────────────────────────────

async fn handle_webhook(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> StatusCode {
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
    let state_name = data.state.as_ref().and_then(|s| s.name.as_deref()).unwrap_or("");
    if state_name != "Todo" {
        return StatusCode::OK;
    }

    let issue = LinearIssue {
        identifier: data.identifier.unwrap_or_else(|| data.id.unwrap_or_default()),
        title: data.title.unwrap_or_default(),
        description: data.description.unwrap_or_default(),
        labels: data
            .labels
            .unwrap_or_default()
            .into_iter()
            .map(|l| l.name)
            .collect(),
    };

    eprintln!("[linear-channel] Forwarding issue {} to Claude", issue.identifier);
    send_channel_notification(&state.stdout, &issue).await;

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
                write_mcp(
                    &stdout,
                    json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                )
                .await;
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

    let state = AppState {
        stdout: stdout.clone(),
    };

    // HTTP server on port 8788 — receives Linear webhooks
    let app = Router::new()
        .route("/webhook", post(handle_webhook))
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
