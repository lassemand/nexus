//! HTTP surface: routes, shared state and the webhook handlers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Router,
};

use crate::{
    github::{
        dispatchable_issue_comment, dispatchable_review_comment, verify_github_signature,
        IssueCommentPayload, PrCommentKind, ReviewCommentPayload,
    },
    linear::{dispatchable_issue, LinearWebhook},
    EventSink, InboundEvent,
};

/// Default bind address. All interfaces, because this runs in a container where
/// the only reachable address is the pod IP; `CONDUCTOR_BIND` overrides it.
pub const DEFAULT_BIND: &str = "0.0.0.0:8788";

/// Header carrying Linear's delivery identifier.
const LINEAR_DELIVERY_HEADER: &str = "Linear-Delivery";
/// Header carrying GitHub's delivery identifier.
const GITHUB_DELIVERY_HEADER: &str = "X-GitHub-Delivery";
/// Header naming the GitHub event type.
const GITHUB_EVENT_HEADER: &str = "X-GitHub-Event";
/// Header carrying GitHub's HMAC signature over the raw body.
const GITHUB_SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// State shared by all handlers.
#[derive(Clone)]
pub struct AppState {
    /// Where accepted events go.
    pub sink: Arc<dyn EventSink>,
    /// Verifies `X-Hub-Signature-256` when set. `None` skips verification —
    /// acceptable for local development, but a real deployment should set it.
    pub github_webhook_secret: Option<String>,
    /// Only comments from these logins are accepted (case-insensitive).
    pub watched_github_users: Vec<String>,
    /// Set once shutdown begins, after which webhooks are refused.
    pub shutting_down: Arc<AtomicBool>,
}

impl AppState {
    /// Whether webhooks should be refused because shutdown has begun.
    fn draining(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }
}

/// Builds the router: both webhook routes plus the health probe.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/webhook", post(handle_linear))
        .route("/webhook/github", post(handle_github))
        .with_state(state)
}

/// Binds `addr` and serves until the process is stopped.
pub async fn serve(addr: &str, state: AppState) -> std::io::Result<()> {
    serve_with_shutdown(addr, state, std::future::pending()).await
}

/// Binds `addr` and serves until `shutdown` resolves.
///
/// Health stays `200` throughout: the pod is still alive while draining, and
/// failing liveness would have Kubernetes kill it mid-run. Readiness is handled
/// by the webhook routes answering `503`.
pub async fn serve_with_shutdown(
    addr: &str,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr, "conductor listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Kubernetes liveness/readiness probe.
///
/// Deliberately depends on nothing but the listener being bound: a probe that
/// consulted downstream state would restart the pod for failures it cannot fix.
async fn health() -> &'static str {
    "ok"
}

/// Reads a delivery identifier, treating a missing or non-ASCII header as absent.
fn delivery_id(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Hands an event to the sink on a detached task, so the HTTP response is not
/// held while it is processed.
fn submit(sink: &Arc<dyn EventSink>, event: InboundEvent) {
    let sink = Arc::clone(sink);
    tokio::spawn(async move { sink.submit(event) });
}

/// `POST /webhook` — Linear.
async fn handle_linear(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    // Accepting now would mean answering 200 for work that will not run.
    if state.draining() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }

    let payload: LinearWebhook = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse Linear payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    if let Some(issue) = dispatchable_issue(payload) {
        submit(
            &state.sink,
            InboundEvent::LinearIssueTodo {
                issue,
                delivery_id: delivery_id(&headers, LINEAR_DELIVERY_HEADER),
            },
        );
    }

    StatusCode::OK
}

/// `POST /webhook/github` — GitHub.
async fn handle_github(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    if state.draining() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }

    // Signature must be checked against the raw bytes, before any parsing.
    if let Some(secret) = &state.github_webhook_secret {
        let signature = headers
            .get(GITHUB_SIGNATURE_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !verify_github_signature(secret, signature, &body) {
            tracing::warn!("GitHub signature verification failed, dropping event");
            return StatusCode::UNAUTHORIZED;
        }
    }

    let delivery = delivery_id(&headers, GITHUB_DELIVERY_HEADER);
    let event = headers
        .get(GITHUB_EVENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    match event {
        "issue_comment" => {
            let payload: IssueCommentPayload = match serde_json::from_slice(&body) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse issue_comment payload");
                    return StatusCode::BAD_REQUEST;
                }
            };
            if let Some(comment) = dispatchable_issue_comment(payload, &state.watched_github_users)
            {
                submit(
                    &state.sink,
                    InboundEvent::GithubPrComment {
                        comment,
                        kind: PrCommentKind::Comment,
                        delivery_id: delivery,
                    },
                );
            }
        }
        "pull_request_review_comment" => {
            let payload: ReviewCommentPayload = match serde_json::from_slice(&body) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse review comment payload");
                    return StatusCode::BAD_REQUEST;
                }
            };
            if let Some(comment) = dispatchable_review_comment(payload, &state.watched_github_users)
            {
                submit(
                    &state.sink,
                    InboundEvent::GithubPrComment {
                        comment,
                        kind: PrCommentKind::ReviewComment,
                        delivery_id: delivery,
                    },
                );
            }
        }
        // Other events are accepted and ignored so the sender does not retry.
        _ => {}
    }

    StatusCode::OK
}
