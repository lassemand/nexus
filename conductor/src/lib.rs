//! Webhook receiver for Linear and GitHub events.
//!
//! `conductor` runs as an ordinary long-running service on the cluster. It
//! accepts webhooks over HTTP, reduces them to [`InboundEvent`]s, and hands those
//! to an [`EventSink`]. Unlike the `mcp` crate it replaces, it is not a child of
//! any Claude Code session, so its lifetime is independent of any consumer and it
//! can eventually serve several sessions at once.
//!
//! Events are dispatched by [`dispatcher::Dispatcher`], one Claude Code session
//! per group, each in its own git worktree. [`sessions`] is the operator's view
//! of those sessions.

pub mod closing;
pub mod dispatcher;
pub mod github;
pub mod github_api;
pub mod hooks;
pub mod http;
pub mod linear;
pub mod linear_api;
pub mod linear_oauth;
pub mod registry;
pub mod resolve;
pub mod review;
pub mod sessions;
pub mod tmux;

use serde::{Deserialize, Serialize};

use github::{GithubPrComment, PrCommentKind, ReviewCommand};
use linear::{ClosedIssue, IssueStateChange, LinearIssue};
use review::ReviewRequest;

/// A webhook that was accepted and parsed, ready to be acted on.
///
/// `delivery_id` carries the provider's delivery header so a future sink can
/// deduplicate retried deliveries. It is `None` when the header was absent,
/// which is not treated as an error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum InboundEvent {
    /// A Linear issue that just transitioned into `Todo`.
    LinearIssueTodo {
        issue: LinearIssue,
        delivery_id: Option<String>,
    },
    /// A Linear issue that became completed, canceled or was removed.
    LinearIssueClosed {
        issue: ClosedIssue,
        delivery_id: Option<String>,
    },
    /// A comment from a watched user on a pull request.
    GithubPrComment {
        comment: GithubPrComment,
        kind: PrCommentKind,
        delivery_id: Option<String>,
    },
    /// Any real state transition of a Linear issue. Drives automatic reviews:
    /// entering the review state starts one, leaving it cancels a running one.
    LinearIssueStateChanged {
        change: IssueStateChange,
        delivery_id: Option<String>,
    },
    /// A `/review` comment from a watched user on a pull request.
    ReviewCommand {
        command: ReviewCommand,
        delivery_id: Option<String>,
    },
    /// A check suite or check run finished on a commit. Drives the CI gate's
    /// re-check of watched issues.
    GithubChecksCompleted {
        repo: String,
        head_sha: String,
        delivery_id: Option<String>,
    },
    /// A push to a pull request (a new head). Resets a CI watch.
    GithubPrPushed {
        repo: String,
        pr_number: u64,
        head_sha: String,
        delivery_id: Option<String>,
    },
    /// A review to run, queued in a review set. Never comes from a webhook.
    ReviewRequested { request: ReviewRequest },
}

/// Destination for accepted events.
///
/// Implementations must not block: the HTTP handler hands events over on a
/// detached task so the webhook sender is never made to wait, and long work here
/// would instead pile up tasks.
pub trait EventSink: Send + Sync + 'static {
    /// Accepts an event. Failures are the sink's own to handle and report.
    fn submit(&self, event: InboundEvent);
}

/// Sink that records one log line per event and does nothing else.
///
/// Placeholder until dispatch exists, and useful on its own for confirming that
/// webhooks arrive and that the transition filter behaves as expected.
pub struct LogSink;

impl EventSink for LogSink {
    fn submit(&self, event: InboundEvent) {
        match event {
            InboundEvent::LinearIssueTodo { issue, delivery_id } => {
                tracing::info!(
                    identifier = %issue.identifier,
                    title = %issue.title,
                    labels = ?issue.labels,
                    session_labels = ?issue.session_labels,
                    delivery_id = ?delivery_id,
                    "linear issue entered Todo"
                );
            }
            InboundEvent::LinearIssueClosed { issue, delivery_id } => {
                tracing::info!(
                    identifier = %issue.identifier,
                    kind = ?issue.kind,
                    session_labels = ?issue.session_labels,
                    delivery_id = ?delivery_id,
                    "linear issue closed"
                );
            }
            InboundEvent::GithubPrComment {
                comment,
                kind,
                delivery_id,
            } => {
                tracing::info!(
                    kind = kind.as_str(),
                    repo = %comment.repo,
                    pr_number = comment.pr_number,
                    comment_id = comment.comment_id,
                    file_path = ?comment.file_path,
                    delivery_id = ?delivery_id,
                    "github pull request comment"
                );
            }
            InboundEvent::LinearIssueStateChanged {
                change,
                delivery_id,
            } => {
                tracing::info!(
                    identifier = %change.identifier,
                    state = %change.state_name,
                    delivery_id = ?delivery_id,
                    "linear issue changed state"
                );
            }
            InboundEvent::ReviewCommand {
                command,
                delivery_id,
            } => {
                tracing::info!(
                    repo = %command.repo,
                    pr_number = command.pr_number,
                    delivery_id = ?delivery_id,
                    "review requested on a pull request"
                );
            }
            InboundEvent::GithubChecksCompleted {
                repo,
                head_sha,
                delivery_id,
            } => {
                tracing::info!(%repo, %head_sha, delivery_id = ?delivery_id, "github checks completed");
            }
            InboundEvent::GithubPrPushed {
                repo,
                pr_number,
                head_sha,
                delivery_id,
            } => {
                tracing::info!(%repo, pr_number, %head_sha, delivery_id = ?delivery_id, "github pull request pushed");
            }
            InboundEvent::ReviewRequested { request } => {
                tracing::info!(identifier = %request.identifier, "review queued");
            }
        }
    }
}
