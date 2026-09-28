//! Standalone webhook server for Linear and GitHub events.
//!
//! Receives webhooks over HTTP, reduces them to [`InboundEvent`]s, and hands
//! those to an [`EventSink`]. It is a plain long-running process: it does not
//! speak MCP and is not a child of any Claude Code session, so its lifetime is
//! independent of any consumer.
//!
//! Dispatching events to consumers is not implemented yet — [`LogSink`] is the
//! only sink, and it just logs.

pub mod github;
pub mod http;
pub mod linear;

use github::{GithubPrComment, PrCommentKind};
use linear::LinearIssue;

/// A webhook that was accepted and parsed, ready to be acted on.
///
/// `delivery_id` carries the provider's delivery header so a future sink can
/// deduplicate retried deliveries. It is `None` when the header was absent,
/// which is not treated as an error.
#[derive(Debug, PartialEq, Eq)]
pub enum InboundEvent {
    /// A Linear issue that just transitioned into `Todo`.
    LinearIssueTodo {
        issue: LinearIssue,
        delivery_id: Option<String>,
    },
    /// A comment from a watched user on a pull request.
    GithubPrComment {
        comment: GithubPrComment,
        event: PrCommentKind,
        delivery_id: Option<String>,
    },
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
            InboundEvent::GithubPrComment {
                comment,
                event,
                delivery_id,
            } => {
                tracing::info!(
                    kind = event.as_str(),
                    repo = %comment.repo,
                    pr_number = comment.pr_number,
                    comment_id = comment.comment_id,
                    file_path = ?comment.file_path,
                    delivery_id = ?delivery_id,
                    "github pull request comment"
                );
            }
        }
    }
}
